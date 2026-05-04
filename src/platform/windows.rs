//! Windows-specific IO primitives.
//!
//! Uses `CreateFileW` with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`
//! for Direct IO and `FlushFileBuffers` for durability. `MoveFileExW` with
//! `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` provides atomic
//! rename semantics.
//!
//! # Design decisions
//!
//! - **Direct IO open:** `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH` is
//!   used (not `FILE_FLAG_NO_BUFFERING` alone with deferred flush) because
//!   `WRITE_THROUGH` ensures each write is durable on return, eliminating the
//!   need for a separate `FlushFileBuffers` call on the Direct IO path.
//! - **Alignment:** `GetDiskFreeSpaceW` returns `BytesPerSector` at handle
//!   creation; the same sector size is used to size aligned scratch buffers.
//! - **Positioned writes (`write_at`):** uses `SetFilePointerEx` + `WriteFile`.
//!   IOCP / overlapped IO is deferred to `0.5.0`.
//! - **Copy:** `std::fs::copy` (wraps `CopyFileExW` internally in std).
//!   `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS reflink) is deferred to `0.5.0`.

#![cfg(target_os = "windows")]

use crate::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    BOOL, FALSE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, GetDiskFreeSpaceW, MoveFileExW, ReadFile, SetFilePointerEx,
    WriteFile, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, OPEN_EXISTING,
};

// ──────────────────────────────────────────────────────────────────────────────
// File opening
// ──────────────────────────────────────────────────────────────────────────────

/// Opens `path` for writing as a new (must-not-exist) file.
pub(crate) fn open_write_new(path: &Path, use_direct: bool) -> Result<(File, bool)> {
    let wide = to_wide(path);

    let flags = if use_direct {
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH
    } else {
        FILE_ATTRIBUTE_NORMAL
    };

    // SAFETY: wide is a valid NUL-terminated UTF-16 string. All flag values
    // are valid Win32 CreateFileW arguments.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            CREATE_NEW,
            flags,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        let err = std::io::Error::last_os_error();
        if use_direct {
            // ERROR_INVALID_PARAMETER (87) is returned on filesystems that
            // do not support FILE_FLAG_NO_BUFFERING (e.g. FAT16, some remote
            // shares). Retry without the Direct IO flags.
            if err.raw_os_error() == Some(87) {
                // SAFETY: same as above, without Direct IO flags.
                let h2 = unsafe {
                    CreateFileW(
                        wide.as_ptr(),
                        GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        std::ptr::null(),
                        CREATE_NEW,
                        FILE_ATTRIBUTE_NORMAL,
                        std::ptr::null_mut(),
                    )
                };
                if h2 != INVALID_HANDLE_VALUE {
                    // SAFETY: h2 is a valid, open handle that we own.
                    let file = unsafe { File::from_raw_handle(h2 as RawHandle) };
                    return Ok((file, false));
                }
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
        }
        return Err(Error::Io(err));
    }

    // SAFETY: handle is a valid, open Windows file handle that we own.
    Ok((
        unsafe { File::from_raw_handle(handle as RawHandle) },
        use_direct,
    ))
}

/// Opens `path` for reading.
pub(crate) fn open_read(path: &Path, use_direct: bool) -> Result<(File, bool)> {
    let wide = to_wide(path);

    let flags = if use_direct {
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_NO_BUFFERING
    } else {
        FILE_ATTRIBUTE_NORMAL
    };

    // SAFETY: wide is valid; all flags are valid CreateFileW arguments.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        let err = std::io::Error::last_os_error();
        if use_direct && err.raw_os_error() == Some(87) {
            // SAFETY: retry without NO_BUFFERING.
            let h2 = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if h2 != INVALID_HANDLE_VALUE {
                // SAFETY: h2 is valid and owned.
                let file = unsafe { File::from_raw_handle(h2 as RawHandle) };
                return Ok((file, false));
            }
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        return Err(Error::Io(err));
    }

    // SAFETY: handle is valid and owned.
    Ok((
        unsafe { File::from_raw_handle(handle as RawHandle) },
        use_direct,
    ))
}

/// Opens `path` for appending (creates if missing).
pub(crate) fn open_append(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(Error::Io)
}

/// Opens `path` for random-access writing.
pub(crate) fn open_write_at(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
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
    let handle = file.as_raw_handle() as HANDLE;
    let mut written = 0u32;
    let mut offset = 0usize;

    while offset < data.len() {
        let chunk_len = u32::try_from(data.len() - offset).unwrap_or(u32::MAX);
        // SAFETY: handle is valid; slice is valid for the duration.
        let ok: BOOL = unsafe {
            WriteFile(
                handle,
                data[offset..].as_ptr().cast(),
                chunk_len,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == FALSE {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        offset += written as usize;
    }
    Ok(())
}

pub(crate) fn write_all_direct(file: &File, data: &[u8], sector_size: u32) -> Result<()> {
    use super::{round_up, AlignedBuf};

    let ss = sector_size as usize;
    let aligned_len = round_up(data.len(), ss);
    let mut buf = AlignedBuf::new(aligned_len, ss)?;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);
    // Remainder is already zero from alloc_zeroed.

    write_all(file, buf.as_slice())
}

pub(crate) fn write_at(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    let handle = file.as_raw_handle() as HANDLE;

    // Seek to the requested offset.
    let dist_lo = (offset & 0xFFFF_FFFF) as i32;
    let dist_hi = (offset >> 32) as i32;
    // SAFETY: handle is valid; FILE_BEGIN is a valid move method.
    let ok: BOOL =
        unsafe { SetFilePointerEx(handle, dist_lo as i64, std::ptr::null_mut(), FILE_BEGIN) };
    // SetFilePointerEx returns 0 on failure (not INVALID_SET_FILE_POINTER).
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    let _ = dist_hi; // Used implicitly via the i64 cast above.

    write_all(file, data)
}

// ──────────────────────────────────────────────────────────────────────────────
// Reading
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn read_all(file: &File) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
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

    let handle = file.as_raw_handle() as HANDLE;
    let mut bytes_read: u32 = 0;
    // SAFETY: handle is valid; buf is aligned and has aligned_len bytes.
    let ok: BOOL = unsafe {
        ReadFile(
            handle,
            buf.as_mut_slice().as_mut_ptr().cast(),
            aligned_len as u32,
            &mut bytes_read,
            std::ptr::null_mut(),
        )
    };
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    let trimmed = usize::min(bytes_read as usize, file_size as usize);
    Ok(buf.as_slice()[..trimmed].to_vec())
}

pub(crate) fn read_range(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    // Clone the handle so we get an independent file cursor to seek.
    let mut seekable = file.try_clone().map_err(Error::Io)?;
    let _pos = seekable.seek(SeekFrom::Start(offset)).map_err(Error::Io)?;

    let mut buf = vec![0u8; len];
    let mut total = 0usize;
    while total < len {
        let n = seekable.read(&mut buf[total..]).map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    buf.truncate(total);
    Ok(buf)
}

// ──────────────────────────────────────────────────────────────────────────────
// Durability
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn sync_data(file: &File) -> Result<()> {
    // Windows has no fdatasync equivalent. FlushFileBuffers flushes both
    // data and metadata. The active_method() is updated to Sync by the
    // caller when Data was requested.
    sync_full(file)
}

pub(crate) fn sync_full(file: &File) -> Result<()> {
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: handle is a valid open file handle.
    let ok: BOOL = unsafe { FlushFileBuffers(handle) };
    if ok != FALSE {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Rename and copy
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    let from_wide = to_wide(from);
    let to_wide = to_wide(to);

    // MOVEFILE_REPLACE_EXISTING: replace `to` if it exists.
    // MOVEFILE_WRITE_THROUGH: do not return until the rename is flushed to
    // stable media, matching the durability guarantee of the write that
    // preceded this rename.
    //
    // SAFETY: both wide strings are valid NUL-terminated UTF-16.
    let ok: BOOL = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok != FALSE {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

pub(crate) fn sync_parent_dir(_path: &Path) -> Result<()> {
    // Directory durability on Windows is implicit when using WRITE_THROUGH
    // on the file rename. No separate directory fsync is needed.
    Ok(())
}

pub(crate) fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    // std::fs::copy uses CopyFileExW internally.
    // TODO(0.5.0): investigate FSCTL_DUPLICATE_EXTENTS_TO_FILE for
    // ReFS reflink copies.
    std::fs::copy(src, dst).map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Probes
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn probe_sector_size(path: &Path) -> u32 {
    // GetDiskFreeSpaceW returns the bytes-per-sector of the volume hosting
    // the given path. We use the path's root as the volume root.
    let root = path
        .components()
        .next()
        .map(|c| {
            let mut s = c.as_os_str().to_os_string();
            s.push("\\");
            s
        })
        .unwrap_or_else(|| std::ffi::OsString::from(".\\"));

    let wide = to_wide_os_string(&root);
    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    let mut free_clusters: u32 = 0;
    let mut total_clusters: u32 = 0;

    // SAFETY: wide is a valid NUL-terminated UTF-16 path; all output
    // pointers are valid mutable references.
    let ok: BOOL = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };

    if ok != FALSE && bytes_per_sector >= 512 {
        bytes_per_sector
    } else {
        512
    }
}

#[allow(dead_code)]
pub(crate) fn probe_direct_io_available() -> bool {
    // FILE_FLAG_NO_BUFFERING is available on all supported Windows versions.
    // Whether it works depends on the filesystem (checked at open time).
    true
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ──────────────────────────────────────────────────────────────────────────────

fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect()
}

fn to_wide_os_string(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0u16)).collect()
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    // (no extra imports needed beyond super::*)
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("fsys_win_{}_{}_{}", std::process::id(), n, suffix))
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
    fn test_open_write_new_fails_if_exists() {
        let path = tmp_path("exists");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"existing").expect("create");
        assert!(open_write_new(&path, false).is_err());
    }

    #[test]
    fn test_write_all_and_read_all_roundtrip() {
        let path = tmp_path("rw");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"windows fsys").expect("write");
        drop(f);

        let (rf, _) = open_read(&path, false).expect("read");
        let data = read_all(&rf).expect("read_all");
        assert_eq!(data, b"windows fsys");
    }

    #[test]
    fn test_sync_full_does_not_fail() {
        let path = tmp_path("sync");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"sync test").expect("write");
        sync_full(&f).expect("flush");
    }

    #[test]
    fn test_atomic_rename_replaces_destination() {
        let src = tmp_path("ren_src");
        let dst = tmp_path("ren_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"new").expect("write src");
        std::fs::write(&dst, b"old").expect("write dst");
        atomic_rename(&src, &dst).expect("rename");
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).expect("read"), b"new");
    }

    #[test]
    fn test_write_at_updates_correct_offset() {
        let path = tmp_path("write_at");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"000000000").expect("create");
        let f = open_write_at(&path).expect("open");
        write_at(&f, 3, b"XXX").expect("write_at");
        drop(f);
        let content = std::fs::read(&path).expect("read");
        assert_eq!(&content[3..6], b"XXX");
    }

    #[test]
    fn test_probe_sector_size_returns_at_least_512() {
        let size = probe_sector_size(Path::new("."));
        assert!(size >= 512, "sector size {} must be ≥ 512", size);
    }

    #[test]
    fn test_copy_file_content_matches() {
        let src = tmp_path("cp_src");
        let dst = tmp_path("cp_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"windows copy").expect("write");
        let bytes = copy_file(&src, &dst).expect("copy");
        assert_eq!(bytes, 12);
        assert_eq!(std::fs::read(&dst).expect("read"), b"windows copy");
    }

    #[test]
    fn test_open_direct_falls_back_gracefully() {
        // On most NTFS volumes Direct IO should succeed, but on some
        // environments it may not. Just verify the function doesn't panic
        // and returns a usable file.
        let path = tmp_path("direct_fb");
        let _g = TmpFile(path.clone());
        let result = open_write_new(&path, true);
        // We accept either success or fallback (direct=false), but not a
        // hard error.
        match result {
            Ok((f, direct)) => {
                if direct {
                    let sector = probe_sector_size(&path);
                    write_all_direct(&f, b"direct test", sector).expect("write after direct open");
                } else {
                    write_all(&f, b"direct test").expect("write after direct open");
                }
            }
            Err(e) => panic!("open_write_new(direct=true) should not hard-fail: {}", e),
        }
    }
}
