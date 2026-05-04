//! macOS-specific IO primitives.
//!
//! Uses `F_NOCACHE` for Direct IO and `F_FULLFSYNC` for durability.
//! Regular `fsync(2)` on macOS only flushes to the drive's write cache —
//! `F_FULLFSYNC` is required to guarantee media durability.

#![cfg(target_os = "macos")]

use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::Path;

// ──────────────────────────────────────────────────────────────────────────────
// File opening
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn open_write_new(path: &Path, use_direct: bool) -> Result<(File, bool)> {
    let path_cstr = path_to_cstr(path)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC;

    // SAFETY: path_cstr is a valid NUL-terminated string; flags and mode are
    // valid open(2) arguments.
    let fd = unsafe { libc::open(path_cstr.as_ptr(), flags, 0o600_i32) };
    if fd < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    if use_direct {
        // F_NOCACHE disables the page cache for this fd.
        // SAFETY: fd is a valid open file descriptor.
        let ret = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1_i32) };
        if ret < 0 {
            // F_NOCACHE failure is rare (some HFS+ configurations). Proceed
            // without it but still use F_FULLFSYNC for durability. The caller
            // will observe the fallback via the returned false flag.
            // TODO(0.3.0): emit a metrics event for this fallback.
            // SAFETY: fd is valid and owned.
            let file = unsafe { File::from_raw_fd(fd) };
            return Ok((file, false));
        }
    }

    // SAFETY: fd is a valid, open file descriptor owned by us.
    Ok((unsafe { File::from_raw_fd(fd) }, use_direct))
}

pub(crate) fn open_read(path: &Path, use_direct: bool) -> Result<(File, bool)> {
    let path_cstr = path_to_cstr(path)?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC;

    // SAFETY: path_cstr and flags are valid.
    let fd = unsafe { libc::open(path_cstr.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    if use_direct {
        // SAFETY: fd is valid and open.
        let ret = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1_i32) };
        if ret < 0 {
            // F_NOCACHE failed; proceed without it.
            // SAFETY: fd is valid.
            let file = unsafe { File::from_raw_fd(fd) };
            return Ok((file, false));
        }
    }

    // SAFETY: fd is valid and owned.
    Ok((unsafe { File::from_raw_fd(fd) }, use_direct))
}

pub(crate) fn open_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(Error::Io)
}

pub(crate) fn open_write_at(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .open(path)
        .map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Writing
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn write_all(file: &File, data: &[u8]) -> Result<()> {
    let fd = file.as_raw_fd();
    let mut written = 0usize;
    while written < data.len() {
        // SAFETY: fd is a valid open file descriptor; the slice is valid.
        let n = unsafe {
            libc::write(
                fd,
                data[written..].as_ptr().cast::<libc::c_void>(),
                data.len() - written,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(err));
        }
        written += n as usize;
    }
    Ok(())
}

pub(crate) fn write_all_direct(file: &File, data: &[u8], sector_size: u32) -> Result<()> {
    use super::{round_up, AlignedBuf};

    // macOS F_NOCACHE does not require strict sector alignment from the
    // application (the kernel handles alignment internally), but we still
    // pad to the sector boundary for consistency with the Linux path.
    let ss = sector_size as usize;
    let aligned_len = round_up(data.len(), ss);
    let mut buf = AlignedBuf::new(aligned_len, ss)?;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);

    let fd = file.as_raw_fd();
    let ptr = buf.as_slice().as_ptr().cast::<libc::c_void>();
    // SAFETY: fd is valid. buf is aligned, zero-padded, and has aligned_len
    // bytes. pwrite does not advance the file position.
    let n = unsafe { libc::pwrite(fd, ptr, aligned_len, 0) };
    if n < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

pub(crate) fn write_at(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    let fd = file.as_raw_fd();
    let mut written = 0usize;
    while written < data.len() {
        let off = (offset as i64).checked_add(written as i64).ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "write_at: offset overflow",
            ))
        })?;
        // SAFETY: fd is valid; the slice is valid for the duration.
        let n = unsafe {
            libc::pwrite(
                fd,
                data[written..].as_ptr().cast::<libc::c_void>(),
                data.len() - written,
                off as libc::off_t,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(err));
        }
        written += n as usize;
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Reading
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn read_all(file: &File) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    // `read_to_end` returns the byte count, redundant with `buf.len()`
    // once the call returns; explicit `_` discard satisfies
    // `unused_results`.
    let _ = (&*file).read_to_end(&mut buf).map_err(Error::Io)?;
    Ok(buf)
}

pub(crate) fn read_all_direct(file: &File, file_size: u64, sector_size: u32) -> Result<Vec<u8>> {
    use super::{round_up, AlignedBuf};

    if file_size == 0 {
        return Ok(Vec::new());
    }

    let ss = sector_size as usize;
    let aligned_len = round_up(file_size as usize, ss);
    let mut buf = AlignedBuf::new(aligned_len, ss)?;

    let fd = file.as_raw_fd();
    let ptr = buf.as_mut_slice().as_mut_ptr().cast::<libc::c_void>();
    // SAFETY: fd is valid; buf is aligned and has aligned_len bytes; offset 0.
    let n = unsafe { libc::pread(fd, ptr, aligned_len, 0) };
    if n < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    let trimmed = usize::min(n as usize, file_size as usize);
    Ok(buf.as_slice()[..trimmed].to_vec())
}

pub(crate) fn read_range(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let fd = file.as_raw_fd();
    let mut buf = vec![0u8; len];
    let mut total_read = 0usize;
    while total_read < len {
        let off = (offset as i64)
            .checked_add(total_read as i64)
            .ok_or_else(|| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "read_range: offset overflow",
                ))
            })?;
        // SAFETY: fd is valid; the slice is valid.
        let n = unsafe {
            libc::pread(
                fd,
                buf[total_read..].as_mut_ptr().cast::<libc::c_void>(),
                len - total_read,
                off as libc::off_t,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(err));
        }
        if n == 0 {
            buf.truncate(total_read);
            break;
        }
        total_read += n as usize;
    }
    buf.truncate(total_read);
    Ok(buf)
}

// ──────────────────────────────────────────────────────────────────────────────
// Durability
//
// On macOS, regular fsync(2) only flushes to the drive's write cache and does
// NOT guarantee media durability. F_FULLFSYNC is the only correct primitive
// for crash-safe writes. This applies to ALL methods on macOS:
//   - Method::Sync:   F_FULLFSYNC
//   - Method::Data:   F_FULLFSYNC (no fdatasync on macOS)
//   - Method::Direct: F_FULLFSYNC (F_NOCACHE + F_FULLFSYNC)
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn sync_data(file: &File) -> Result<()> {
    // macOS has no fdatasync equivalent. Use F_FULLFSYNC for correctness.
    sync_full(file)
}

pub(crate) fn sync_full(file: &File) -> Result<()> {
    let fd = file.as_raw_fd();
    // F_FULLFSYNC forces the drive to flush its write cache to stable media.
    // This is the only durable sync primitive on macOS.
    //
    // SAFETY: fd is a valid open file descriptor.
    let ret = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC, 0_i32) };
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Rename, directory sync, copy
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    // POSIX rename(2) is atomic within the same filesystem.
    // renameatx_np(RENAME_SWAP) would swap two existing files, which is NOT
    // what we need — we want to replace the destination. Plain rename() is
    // the correct primitive. See .dev/DECISIONS-0.3.0.md.
    std::fs::rename(from, to).map_err(Error::Io)
}

pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = File::open(parent).map_err(Error::Io)?;
    let fd = dir.as_raw_fd();
    // Use F_FULLFSYNC on the directory as well for full durability.
    // SAFETY: fd is a valid open directory file descriptor.
    let ret = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC, 0_i32) };
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

pub(crate) fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    // TODO(0.5.0): use clonefile(2) for instant, copy-on-write file cloning
    // on APFS. Falls back to std::fs::copy for now.
    std::fs::copy(src, dst).map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Probes
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn probe_sector_size(path: &Path) -> u32 {
    use libc::statfs;

    let path_cstr = match path_to_cstr(path) {
        Ok(c) => c,
        Err(_) => return 512,
    };

    let mut st: statfs = unsafe { std::mem::zeroed() };
    // SAFETY: path_cstr is valid NUL-terminated; st is properly sized.
    let ret = unsafe { libc::statfs(path_cstr.as_ptr(), &mut st) };
    if ret == 0 && st.f_bsize > 0 {
        let bs = st.f_bsize as u64;
        if bs >= 512 && bs <= 65536 {
            return bs as u32;
        }
    }
    512
}

pub(crate) fn probe_direct_io_available() -> bool {
    // F_NOCACHE is available on all macOS versions supported by fsys.
    true
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal helper
// ──────────────────────────────────────────────────────────────────────────────

fn path_to_cstr(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::InvalidPath {
        path: path.to_owned(),
        reason: "path contains an interior NUL byte".into(),
    })
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_macos_{}_{}_{}",
            std::process::id(),
            n,
            suffix
        ))
    }

    struct TmpFile(std::path::PathBuf);
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_open_write_new_creates_file() {
        let path = tmp_path("create");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        drop(f);
        assert!(path.exists());
    }

    #[test]
    fn test_write_read_roundtrip() {
        let path = tmp_path("rw");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"macos").expect("write");
        drop(f);
        let (rf, _) = open_read(&path, false).expect("read open");
        let data = read_all(&rf).expect("read");
        assert_eq!(data, b"macos");
    }

    #[test]
    fn test_sync_full_does_not_panic() {
        let path = tmp_path("sync");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"sync test").expect("write");
        sync_full(&f).expect("sync_full");
    }

    #[test]
    fn test_atomic_rename_works() {
        let src = tmp_path("ren_src");
        let dst = tmp_path("ren_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"new content").expect("write");
        atomic_rename(&src, &dst).expect("rename");
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).expect("read"), b"new content");
    }

    #[test]
    fn test_probe_sector_size_returns_at_least_512() {
        let size = probe_sector_size(Path::new("/tmp"));
        assert!(size >= 512);
    }

    #[test]
    fn test_copy_file_produces_correct_content() {
        let src = tmp_path("cp_src");
        let dst = tmp_path("cp_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"copy content").expect("write");
        let bytes = copy_file(&src, &dst).expect("copy");
        assert_eq!(bytes, 12);
    }
}
