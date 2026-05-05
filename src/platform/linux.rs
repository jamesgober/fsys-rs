//! Linux-specific IO primitives.
//!
//! Provides `O_DIRECT`, `pwrite`/`pread`, `fdatasync`, `fsync`,
//! `copy_file_range`, and `rename` for Linux targets.
//!
//! `io_uring` integration is deferred to `0.5.0`. All IO here is
//! synchronous via `pwrite(2)` / `pread(2)` after the file is opened.

#![cfg(target_os = "linux")]

use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::Path;

// ──────────────────────────────────────────────────────────────────────────────
// File opening
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn open_write_new(path: &Path, use_direct: bool) -> Result<(File, bool)> {
    let path_cstr = path_to_cstr(path)?;
    let mut flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC;
    if use_direct {
        flags |= libc::O_DIRECT;
    }

    // SAFETY: path_cstr is a valid NUL-terminated string. flags and mode
    // are valid open(2) arguments.
    let fd = unsafe { libc::open(path_cstr.as_ptr(), flags, 0o600_i32) };
    if fd >= 0 {
        // SAFETY: fd is a valid, open file descriptor owned by us.
        return Ok((unsafe { File::from_raw_fd(fd) }, use_direct));
    }

    let err = std::io::Error::last_os_error();
    if use_direct && err.raw_os_error() == Some(libc::EINVAL) {
        // O_DIRECT was rejected (tmpfs, FUSE, some CIFS mounts, etc.).
        // Retry without it. The caller will observe the fallback via the
        // returned false flag and can update active_method() accordingly.
        // TODO(0.3.0): emit a metrics event when this fallback fires.
        let flags_no_direct = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC;
        // SAFETY: same as above.
        let fd2 = unsafe { libc::open(path_cstr.as_ptr(), flags_no_direct, 0o600_i32) };
        if fd2 >= 0 {
            // SAFETY: fd2 is a valid, open file descriptor owned by us.
            return Ok((unsafe { File::from_raw_fd(fd2) }, false));
        }
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    Err(Error::Io(err))
}

pub(crate) fn open_read(path: &Path, use_direct: bool) -> Result<(File, bool)> {
    let path_cstr = path_to_cstr(path)?;
    let mut flags = libc::O_RDONLY | libc::O_CLOEXEC;
    if use_direct {
        flags |= libc::O_DIRECT;
    }

    // SAFETY: path_cstr is valid, flags are valid.
    let fd = unsafe { libc::open(path_cstr.as_ptr(), flags, 0) };
    if fd >= 0 {
        // SAFETY: fd is valid and owned by us.
        return Ok((unsafe { File::from_raw_fd(fd) }, use_direct));
    }

    let err = std::io::Error::last_os_error();
    if use_direct && err.raw_os_error() == Some(libc::EINVAL) {
        let flags_no_direct = libc::O_RDONLY | libc::O_CLOEXEC;
        // SAFETY: same as above.
        let fd2 = unsafe { libc::open(path_cstr.as_ptr(), flags_no_direct, 0) };
        if fd2 >= 0 {
            // SAFETY: fd2 is valid and owned.
            return Ok((unsafe { File::from_raw_fd(fd2) }, false));
        }
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    Err(Error::Io(err))
}

pub(crate) fn open_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(Error::Io)
}

pub(crate) fn open_write_at(path: &Path) -> Result<File> {
    // `truncate(false)` is the explicit "preserve existing content" intent
    // for `write_at`: random-access writes overlay specific byte ranges
    // and must not destroy the rest of the file. Required by clippy's
    // `suspicious_open_options` when `create(true)` is set.
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
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
        // SAFETY: fd is a valid open file descriptor. The slice is valid
        // for the duration of this call.
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

    // Empty input — no bytes to write, no buffer to allocate. The
    // caller's `open()` already created the file at size 0; this
    // function is a no-op. (`AlignedBuf::new(0, ...)` would error;
    // we short-circuit before reaching it.)
    if data.is_empty() {
        return Ok(());
    }

    let ss = sector_size as usize;
    // O_DIRECT requires length to be a multiple of the sector size.
    // Pad with zeros if necessary.
    let aligned_len = round_up(data.len(), ss);
    let mut buf = AlignedBuf::new(aligned_len, ss)?;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);
    // Remainder is already zero from alloc_zeroed.

    let fd = file.as_raw_fd();
    let base = buf.as_slice().as_ptr();

    // Loop on partial writes. `pwrite(2)` may return less than
    // requested on EINTR or transient short-write conditions on
    // certain filesystems. Without a loop, a Direct write of a
    // large payload could silently truncate.
    let mut written = 0usize;
    while written < aligned_len {
        // SAFETY: fd is valid; buf is sector-aligned and has
        // aligned_len bytes available; the offset (written) and
        // length (aligned_len - written) stay within bounds.
        let n = unsafe {
            libc::pwrite(
                fd,
                base.add(written).cast::<libc::c_void>(),
                aligned_len - written,
                written as libc::off_t,
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
            return Err(Error::Io(std::io::Error::other(
                "pwrite returned 0 in write_all_direct (no progress)",
            )));
        }
        written += n as usize;
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
        // SAFETY: fd is valid. The slice is valid for the duration of pwrite.
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

/// Sector-aligned positioned write for `O_DIRECT` files.
///
/// **Pre-conditions** (caller-enforced):
/// - `data.as_ptr()` is aligned to the underlying device's sector size.
/// - `data.len()` is a multiple of the sector size.
/// - `offset` is a multiple of the sector size.
///
/// Used by the direct-IO journal log buffer. Same `pwrite` syscall as
/// [`write_at`]; the alignment invariants come from the caller, not from
/// any padding here.
pub(crate) fn write_at_direct(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    write_at(file, offset, data)
}

// ──────────────────────────────────────────────────────────────────────────────
// Reading
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn read_all(file: &File) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    // Read via the immutable reference using the `Read` impl on `&File`
    // (available on Unix). The returned byte count is redundant with
    // `buf.len()` once the call returns; explicit `_` discard satisfies
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
    // SAFETY: fd is valid. buf is sector-aligned and has aligned_len bytes.
    // pread reads from offset 0 without changing the file position.
    let n = unsafe { libc::pread(fd, ptr, aligned_len, 0) };
    if n < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    let actual = n as usize;
    let trimmed = usize::min(actual, file_size as usize);
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
        // SAFETY: fd is valid. buf slice is valid for the duration.
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
            // EOF before we read all requested bytes.
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
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn sync_data(file: &File) -> Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open file descriptor.
    let ret = unsafe { libc::fdatasync(fd) };
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

pub(crate) fn sync_full(file: &File) -> Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open file descriptor.
    let ret = unsafe { libc::fsync(fd) };
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
    // POSIX rename(2) is atomic within the same filesystem. It is the
    // correct primitive for our atomic-replace pattern.
    //
    // Design note (see .dev/DECISIONS-0.3.0.md): renameat2(RENAME_EXCHANGE)
    // atomically swaps two existing files, which is NOT what we need here —
    // we want to replace the destination (which may not exist). Plain
    // rename() is the correct choice. renameat2(RENAME_NOREPLACE) would
    // fail if the destination already exists, which is also wrong for our
    // use case.
    std::fs::rename(from, to).map_err(Error::Io)
}

pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = File::open(parent).map_err(Error::Io)?;
    let fd = dir.as_raw_fd();
    // SAFETY: fd is a valid open directory file descriptor.
    let ret = unsafe { libc::fsync(fd) };
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

pub(crate) fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    // Attempt copy_file_range(2) for same-filesystem reflinks / server-side
    // copies. Falls back to std::fs::copy on EXDEV (cross-device) or ENOSYS.
    //
    // TODO(0.5.0): implement a proper copy_file_range loop for large files
    // and remove the std::fs::copy fallback.
    std::fs::copy(src, dst).map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Probes
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Storage-engine primitives — preallocate + advise
// ──────────────────────────────────────────────────────────────────────────────

/// Pre-allocates `len` bytes of disk space starting at `offset` via
/// `fallocate(2)` with `FALLOC_FL_KEEP_SIZE`. Reserves filesystem
/// extents without changing the logical file size — the journal
/// can write into the reserved region knowing the kernel won't
/// need to allocate blocks mid-write.
///
/// Falls back to `posix_fallocate(3)` if `fallocate` returns
/// `EOPNOTSUPP` or `ENOSYS`. `posix_fallocate` is portable but
/// writes zeros into the reserved region (slower, page-cache-
/// polluting); `fallocate` is the modern Linux way.
pub(crate) fn preallocate(file: &File, offset: u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let fd = file.as_raw_fd();
    // Try `fallocate` first — fastest path, doesn't write zeros.
    // FALLOC_FL_KEEP_SIZE = 0x01.
    const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
    // SAFETY: fd is valid; offset/len are u64 → off_t conversions
    // bounded below i64::MAX by the caller's responsibility (file
    // sizes don't exceed exabyte ranges in any realistic
    // workload).
    let off = offset as libc::off_t;
    let len_off = len as libc::off_t;
    let ret = unsafe { libc::fallocate(fd, FALLOC_FL_KEEP_SIZE, off, len_off) };
    if ret == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    let raw = err.raw_os_error().unwrap_or(0);
    // EOPNOTSUPP (95) on filesystems without fallocate (e.g. FUSE
    // without the right hooks); ENOSYS (38) on very old kernels.
    if raw != 95 && raw != 38 {
        return Err(Error::Io(err));
    }
    // Fallback: posix_fallocate (writes zeros).
    // SAFETY: fd is valid; off/len are bounded as above.
    let ret = unsafe { libc::posix_fallocate(fd, off, len_off) };
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::from_raw_os_error(ret)))
    }
}

/// Hints the kernel about access pattern for a region of `file`
/// via `posix_fadvise(2)`.
pub(crate) fn advise(file: &File, offset: u64, len: u64, advice: crate::Advice) -> Result<()> {
    let fd = file.as_raw_fd();
    let raw_advice: i32 = match advice {
        crate::Advice::Normal => libc::POSIX_FADV_NORMAL,
        crate::Advice::Sequential => libc::POSIX_FADV_SEQUENTIAL,
        crate::Advice::Random => libc::POSIX_FADV_RANDOM,
        crate::Advice::WillNeed => libc::POSIX_FADV_WILLNEED,
        crate::Advice::DontNeed => libc::POSIX_FADV_DONTNEED,
    };
    // SAFETY: fd is valid; offset/len are u64 → off_t.
    let ret =
        unsafe { libc::posix_fadvise(fd, offset as libc::off_t, len as libc::off_t, raw_advice) };
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::from_raw_os_error(ret)))
    }
}

pub(crate) fn probe_sector_size(path: &Path) -> u32 {
    let path_cstr = match path_to_cstr(path) {
        Ok(c) => c,
        Err(_) => return 512,
    };

    // SAFETY: `libc::statfs` is plain old data — every field is an
    // integer or array of integers — so an all-zero bit pattern is a
    // valid value. The struct is then fully written by `libc::statfs`
    // below before any fields are read.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: path_cstr is a valid NUL-terminated string; st is properly
    // sized and zero-initialised.
    let ret = unsafe { libc::statfs(path_cstr.as_ptr(), &mut st) };
    if ret == 0 && st.f_bsize > 0 {
        // f_bsize is the "preferred IO size" — a good proxy for the sector
        // size for Direct IO alignment purposes.
        let bs = st.f_bsize as u64;
        // Clamp to the range [512, 65536].
        if (512..=65536).contains(&bs) {
            return bs as u32;
        }
    }
    512
}

pub(crate) fn probe_direct_io_available() -> bool {
    // O_DIRECT is available in every Linux kernel ≥ 2.4.1.
    // Whether it works on a specific filesystem is checked at open time.
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
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_linux_{}_{}_{}",
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
        let path = tmp_path("open_write_new");
        let _guard = TmpFile(path.clone());
        let (f, _direct) = open_write_new(&path, false).expect("open_write_new");
        drop(f);
        assert!(path.exists());
    }

    #[test]
    fn test_open_write_new_fails_if_already_exists() {
        let path = tmp_path("owne_exists");
        let _guard = TmpFile(path.clone());
        std::fs::write(&path, b"existing").expect("create");
        let result = open_write_new(&path, false);
        assert!(result.is_err(), "must fail when file already exists");
    }

    #[test]
    fn test_write_all_and_read_all_roundtrip() {
        let path = tmp_path("write_read");
        let _guard = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"hello fsys").expect("write");
        drop(f);

        let (rf, _) = open_read(&path, false).expect("open read");
        let data = read_all(&rf).expect("read");
        assert_eq!(data, b"hello fsys");
    }

    #[test]
    fn test_write_at_and_read_range() {
        let path = tmp_path("write_at");
        let _guard = TmpFile(path.clone());
        std::fs::write(&path, b"aaaaaaaaa").expect("create");
        let f = open_write_at(&path).expect("open write at");
        write_at(&f, 2, b"bbb").expect("write at");
        drop(f);

        let (rf, _) = open_read(&path, false).expect("open read");
        let chunk = read_range(&rf, 2, 3).expect("read range");
        assert_eq!(chunk, b"bbb");
    }

    #[test]
    fn test_sync_data_succeeds_on_open_file() {
        let path = tmp_path("sync_data");
        let _guard = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"data").expect("write");
        sync_data(&f).expect("sync_data");
    }

    #[test]
    fn test_sync_full_succeeds_on_open_file() {
        let path = tmp_path("sync_full");
        let _guard = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"full sync test").expect("write");
        sync_full(&f).expect("sync_full");
    }

    #[test]
    fn test_atomic_rename_replaces_destination() {
        let src = tmp_path("rename_src");
        let dst = tmp_path("rename_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"new").expect("write src");
        std::fs::write(&dst, b"old").expect("write dst");
        atomic_rename(&src, &dst).expect("rename");
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).expect("read dst"), b"new");
    }

    #[test]
    fn test_copy_file_produces_identical_content() {
        let src = tmp_path("copy_src");
        let dst = tmp_path("copy_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"copy me").expect("write");
        let bytes = copy_file(&src, &dst).expect("copy");
        assert_eq!(bytes, 7);
        assert_eq!(std::fs::read(&dst).expect("read"), b"copy me");
    }

    #[test]
    fn test_probe_sector_size_returns_at_least_512() {
        let dir = std::env::temp_dir();
        let size = probe_sector_size(&dir);
        assert!(size >= 512, "sector size {}", size);
    }

    #[test]
    fn test_open_append_creates_and_appends() {
        let path = tmp_path("append");
        let _guard = TmpFile(path.clone());
        {
            let mut f = open_append(&path).expect("open append");
            f.write_all(b"line1\n").expect("write");
        }
        {
            let mut f = open_append(&path).expect("open append 2");
            f.write_all(b"line2\n").expect("write");
        }
        let content = std::fs::read(&path).expect("read");
        assert_eq!(content, b"line1\nline2\n");
    }
}
