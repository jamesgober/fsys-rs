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

/// 0.9.4 — Sets the per-file NVMe write-lifetime hint via
/// `fcntl(F_SET_RW_HINT)` (kernel ≥ 4.13).
///
/// Linux exposes four predefined hint values; we accept a
/// 0-based discriminant matching the order
/// `Short / Medium / Long / Extreme` and convert to the kernel
/// constants `RWH_WRITE_LIFE_{SHORT,MEDIUM,LONG,EXTREME}`
/// (`1, 2, 3, 4`).
///
/// Failure is non-fatal — kernels older than 4.13, drives
/// without multi-stream support, or filesystems that reject
/// the fcntl will return an error which the journal-open path
/// swallows. The hint is advisory; missing it costs at most
/// some NAND-GC efficiency, never correctness.
pub(crate) fn fcntl_set_rw_hint(file: &File, hint_ordinal: u8) -> Result<()> {
    // kernel uapi:
    //   #define F_SET_RW_HINT   1036
    //   RWH_WRITE_LIFE_NOT_SET = 0
    //   RWH_WRITE_LIFE_NONE    = 1
    //   RWH_WRITE_LIFE_SHORT   = 2
    //   RWH_WRITE_LIFE_MEDIUM  = 3
    //   RWH_WRITE_LIFE_LONG    = 4
    //   RWH_WRITE_LIFE_EXTREME = 5
    // We re-order our enum so Short=0..Extreme=3 maps to kernel
    // SHORT(2)..EXTREME(5) by adding 2.
    const F_SET_RW_HINT: libc::c_int = 1036;
    let kernel_hint: u64 = (hint_ordinal as u64).saturating_add(2);
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open file descriptor; the third
    // argument to F_SET_RW_HINT is a `u64 *` (per uapi headers).
    let ret = unsafe { libc::fcntl(fd, F_SET_RW_HINT, &kernel_hint as *const u64) };
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
    let off = offset as libc::off_t;
    let len_off = len as libc::off_t;
    // SAFETY: fd is a valid file descriptor owned by `file`;
    // off/len are u64 → off_t conversions bounded below i64::MAX
    // by the caller's responsibility (file sizes don't exceed
    // exabyte ranges in any realistic workload).
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

// ──────────────────────────────────────────────────────────────────────────────
// 0.9.5 — Hole punch + zero range + fiemap extent mapping
// ──────────────────────────────────────────────────────────────────────────────

/// 0.9.5 — A single contiguous extent reported by the `FIEMAP`
/// ioctl. Maps a logical byte range within a file to its physical
/// byte position on the underlying block device.
///
/// Constructed by [`fiemap_extents`]. The `physical` byte offset
/// divided by the device's logical sector size gives the
/// **starting LBA** of the extent — that's the value the NVMe
/// Dataset Management Deallocate command needs.
///
/// `#[allow(dead_code)]`: forward-looking helper. The NVMe DSM
/// Deallocate submission path that consumes these extents is
/// the legitimate architectural-dep deferral from the 0.9.5
/// scope discussion — the helper is in place so the future
/// release can wire it without re-litigating the extent-flag
/// safety model.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct FiemapExtent {
    /// File offset (in bytes) of the first byte of this extent.
    pub logical: u64,
    /// Device byte offset of the first byte. Divide by
    /// `logical_sector_size` to get the starting LBA.
    pub physical: u64,
    /// Length of the extent in bytes. Always a multiple of the
    /// filesystem's allocation unit on well-aligned extents.
    pub length: u64,
    /// Raw `FIEMAP_EXTENT_*` flag bits as returned by the kernel.
    /// See [`fiemap_extent_is_usable_for_dsm`] for the canonical
    /// "is this extent safe to issue DSM Deallocate against" check.
    pub flags: u32,
}

// `#[allow(dead_code)]` on each constant: same justification as
// `FiemapExtent` above — the NVMe DSM Deallocate submission path
// that would dispatch on these flag bits is a future-release
// architectural item; the flag table is in place now so the
// future wiring is mechanical, not exploratory.
/// `FIEMAP_EXTENT_LAST` — the kernel sets this on the final
/// extent returned for the requested range.
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
/// `FIEMAP_EXTENT_UNKNOWN` — extent's physical location is
/// unknown to the kernel.
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
/// `FIEMAP_EXTENT_DELALLOC` — data is delayed-allocated; no
/// physical mapping yet.
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
/// `FIEMAP_EXTENT_ENCODED` — extent is compressed or encoded;
/// physical mapping does not correspond to raw data bytes.
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_ENCODED: u32 = 0x0000_0008;
/// `FIEMAP_EXTENT_DATA_ENCRYPTED` — extent is encrypted.
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_DATA_ENCRYPTED: u32 = 0x0000_0080;
/// `FIEMAP_EXTENT_NOT_ALIGNED` — physical alignment unknown
/// (e.g., XFS-style real-time subvolume).
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_NOT_ALIGNED: u32 = 0x0000_0100;
/// `FIEMAP_EXTENT_DATA_INLINE` — data is inlined in the inode;
/// no separate block allocation.
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0000_0200;
/// `FIEMAP_EXTENT_DATA_TAIL` — data is packed with other items
/// (e.g., ReiserFS tail packing).
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_DATA_TAIL: u32 = 0x0000_0400;
/// `FIEMAP_EXTENT_UNWRITTEN` — block is allocated but contains
/// no written data yet (returns zeros on read).
#[allow(dead_code)]
pub(crate) const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x0000_0800;

/// 0.9.5 — Returns `true` if the extent is safe to issue an NVMe
/// Dataset Management Deallocate against.
///
/// We require:
/// - A stable physical mapping (`!UNKNOWN`, `!DELALLOC`,
///   `!NOT_ALIGNED`).
/// - No encoding / compression / encryption (`!ENCODED`,
///   `!DATA_ENCRYPTED`).
/// - Not an inline / tail-packed extent (`!DATA_INLINE`,
///   `!DATA_TAIL`).
/// - Already written data (`!UNWRITTEN` — unwritten extents have
///   no real LBA content to deallocate; the kernel returns
///   zeros on read regardless).
///
/// Skipped extents are still freed by the `fallocate(PUNCH_HOLE)`
/// step of `punch_hole`; we just don't issue NVMe DSM for them
/// (which would be a meaningless operation against a region the
/// drive doesn't have an LBA mapping for).
#[allow(dead_code)]
#[inline]
pub(crate) fn fiemap_extent_is_usable_for_dsm(flags: u32) -> bool {
    const UNUSABLE: u32 = FIEMAP_EXTENT_UNKNOWN
        | FIEMAP_EXTENT_DELALLOC
        | FIEMAP_EXTENT_ENCODED
        | FIEMAP_EXTENT_DATA_ENCRYPTED
        | FIEMAP_EXTENT_NOT_ALIGNED
        | FIEMAP_EXTENT_DATA_INLINE
        | FIEMAP_EXTENT_DATA_TAIL
        | FIEMAP_EXTENT_UNWRITTEN;
    (flags & UNUSABLE) == 0
}

/// 0.9.5 — Issues `FS_IOC_FIEMAP` against `fd` for the byte
/// range `[start, start + length)` and returns the list of
/// extents the kernel reports.
///
/// The returned vector is in ascending logical-offset order.
/// Callers that want only DSM-safe extents should filter via
/// [`fiemap_extent_is_usable_for_dsm`].
///
/// Implementation detail: makes up to 4 ioctl calls in a row,
/// each requesting 64 extents. Most files map to under 64
/// extents (filesystems coalesce contiguous allocations
/// aggressively); 256 is a reasonable practical ceiling without
/// unbounded buffer growth. Files with > 256 extents in the
/// requested range have their extent list truncated; the
/// truncation is observable because the last extent's
/// `FIEMAP_EXTENT_LAST` flag will not be set.
///
/// # Errors
///
/// - [`Error::Io`] wrapping the ioctl errno on failure (commonly
///   `EOPNOTSUPP` on filesystems that don't implement
///   `FIEMAP` — tmpfs, FUSE, etc.).
#[allow(dead_code)]
pub(crate) fn fiemap_extents(
    fd: std::os::unix::io::RawFd,
    start: u64,
    length: u64,
) -> Result<Vec<FiemapExtent>> {
    /// Kernel uapi `struct fiemap_extent` layout — 56 bytes.
    /// All fields little-endian on x86_64 / aarch64.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct KernelFiemapExtent {
        fe_logical: u64,
        fe_physical: u64,
        fe_length: u64,
        fe_reserved64: [u64; 2],
        fe_flags: u32,
        fe_reserved: [u32; 3],
    }
    /// Kernel uapi `struct fiemap` (header). 32 bytes.
    #[repr(C)]
    #[derive(Default)]
    struct KernelFiemapHeader {
        fm_start: u64,
        fm_length: u64,
        fm_flags: u32,
        fm_mapped_extents: u32,
        fm_extent_count: u32,
        fm_reserved: u32,
    }
    /// FS_IOC_FIEMAP = _IOWR('f' = 0x66, 11, struct fiemap)
    /// = (3 << 30) | (32 << 16) | (0x66 << 8) | 11 = 0xc020660b.
    const FS_IOC_FIEMAP: libc::c_ulong = 0xc020_660b;
    /// Default per-call extent batch size. 64 covers the
    /// overwhelming majority of files in one ioctl; we loop up
    /// to 4 times for files with more extents.
    const EXTENTS_PER_CALL: u32 = 64;
    const MAX_CALLS: usize = 4;

    let mut out: Vec<FiemapExtent> = Vec::new();
    let mut current_start = start;
    let mut remaining = length;

    for _call in 0..MAX_CALLS {
        if remaining == 0 {
            break;
        }
        // Allocate a contiguous buffer for the header + extent
        // array. Layout: [fiemap_header][fiemap_extent; N].
        let header_size = std::mem::size_of::<KernelFiemapHeader>();
        let extent_size = std::mem::size_of::<KernelFiemapExtent>();
        let buf_size = header_size + (EXTENTS_PER_CALL as usize) * extent_size;
        // Heap allocation aligned to u64 boundary (the kernel
        // touches u64-aligned fields in the header).
        let mut buf: Vec<u8> = vec![0u8; buf_size];

        // Populate the header.
        // SAFETY: `buf` is at least `header_size` bytes and `Vec<u8>`
        // is allocated with alignment ≥ alignment_of::<u8>() = 1;
        // we cast through `*mut KernelFiemapHeader` and only access
        // fields via `ptr::write` (no unaligned reads through a
        // typed reference).
        unsafe {
            let header_ptr = buf.as_mut_ptr() as *mut KernelFiemapHeader;
            std::ptr::write(
                header_ptr,
                KernelFiemapHeader {
                    fm_start: current_start,
                    fm_length: remaining,
                    fm_flags: 0,
                    fm_mapped_extents: 0,
                    fm_extent_count: EXTENTS_PER_CALL,
                    fm_reserved: 0,
                },
            );
        }

        // SAFETY: `fd` is a valid open file descriptor for the
        // duration of this call. `buf.as_mut_ptr()` points to
        // `buf_size` bytes of valid memory. The kernel reads the
        // header in-place and writes up to `EXTENTS_PER_CALL`
        // entries into the array that follows.
        let rc = unsafe { libc::ioctl(fd, FS_IOC_FIEMAP, buf.as_mut_ptr() as *mut libc::c_void) };
        if rc < 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }

        // Read back the header to find how many extents the
        // kernel actually populated.
        // SAFETY: header is at the start of the buffer; we wrote
        // a valid KernelFiemapHeader there before the ioctl, and
        // the kernel updates only the in-out fields per the
        // FIEMAP contract.
        let mapped = unsafe {
            let header_ptr = buf.as_ptr() as *const KernelFiemapHeader;
            std::ptr::read(header_ptr).fm_mapped_extents
        };
        if mapped == 0 {
            break;
        }

        // Pull out each extent. Track the last extent's end
        // position so we can advance `current_start` for the
        // next call if needed.
        let mut last_logical_end: u64 = current_start;
        let mut hit_last = false;
        for i in 0..(mapped as usize).min(EXTENTS_PER_CALL as usize) {
            // SAFETY: each extent is `extent_size` bytes after
            // the header at index `i`. We read via `ptr::read`
            // (handles unaligned reads on platforms where it
            // matters; on x86_64 / aarch64 the buffer happens to
            // be naturally aligned at the start).
            let extent: KernelFiemapExtent = unsafe {
                let ext_ptr =
                    buf.as_ptr().add(header_size + i * extent_size) as *const KernelFiemapExtent;
                std::ptr::read_unaligned(ext_ptr)
            };
            last_logical_end = extent.fe_logical.saturating_add(extent.fe_length);
            if (extent.fe_flags & FIEMAP_EXTENT_LAST) != 0 {
                hit_last = true;
            }
            out.push(FiemapExtent {
                logical: extent.fe_logical,
                physical: extent.fe_physical,
                length: extent.fe_length,
                flags: extent.fe_flags,
            });
        }

        if hit_last {
            break;
        }
        // Advance for the next call. Bail if we didn't make
        // forward progress (paranoid against pathological
        // filesystems).
        if last_logical_end <= current_start {
            break;
        }
        let advanced = last_logical_end - current_start;
        if advanced >= remaining {
            break;
        }
        current_start = last_logical_end;
        remaining -= advanced;
    }

    Ok(out)
}

/// 0.9.5 — Punches a hole in `file` at `[offset, offset + len)`.
///
/// Calls `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`
/// — the canonical Linux primitive for hole-punching. After this
/// call the byte range still exists logically (the file size is
/// unchanged) but reads from it return zeros, and the underlying
/// blocks are returned to the filesystem free pool. Most modern
/// filesystems (ext4 with `discard`, xfs, btrfs) issue NVMe TRIM
/// to the device automatically as part of the operation.
///
/// Returns `Ok(())` on success. Returns an `Err` wrapping
/// `EOPNOTSUPP` on filesystems that don't support `FALLOC_FL_PUNCH_HOLE`
/// (rare on modern Linux — ext2 / vfat / certain FUSE mounts).
/// Callers building cross-platform code should treat the error
/// as a feature gap (the caller's data is unchanged) rather than
/// as a hard failure.
pub(crate) fn punch_hole(file: &File, offset: u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let fd = file.as_raw_fd();
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    // SAFETY: fd is a valid open file descriptor. fallocate(2)
    // returns 0 on success and -1 on error with errno set.
    let rc = unsafe { libc::fallocate(fd, mode, offset as i64, len as i64) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

/// 0.9.5 — Zero-fills `file` at `[offset, offset + len)`.
///
/// Calls `fallocate(FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE)`.
/// On capable filesystems + NVMe drives the kernel translates
/// this to an NVMe `WRITE ZEROES` command — the drive controller
/// marks the range as zeros without any host→device data
/// transfer. Falls back to a regular write-of-zeros on
/// filesystems that don't implement `FALLOC_FL_ZERO_RANGE`.
///
/// Returns `Ok(())` on success. Returns an `Err` wrapping
/// `EOPNOTSUPP` on filesystems that don't support
/// `FALLOC_FL_ZERO_RANGE`.
pub(crate) fn zero_range(file: &File, offset: u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let fd = file.as_raw_fd();
    let mode = libc::FALLOC_FL_ZERO_RANGE | libc::FALLOC_FL_KEEP_SIZE;
    // SAFETY: fd is a valid open file descriptor.
    let rc = unsafe { libc::fallocate(fd, mode, offset as i64, len as i64) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}
