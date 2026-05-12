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
//! - **Positioned writes (`write_at`):** uses `WriteFile` with an
//!   `OVERLAPPED` struct carrying the offset (Windows' equivalent of
//!   POSIX `pwrite`). Concurrent-safe at the same fd because the
//!   per-fd cursor is not consulted for the write position.
//!   (0.8.0 R-1 tier-2 fix; earlier versions used SetFilePointerEx +
//!   WriteFile which raced on the cursor under multi-thread append.)
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
    CreateFileW, FlushFileBuffers, GetDiskFreeSpaceW, MoveFileExW, ReadFile, WriteFile, CREATE_NEW,
    FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ,
    FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

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

    // Empty input — no-op. See linux.rs::write_all_direct for the
    // rationale (AlignedBuf::new rejects size=0).
    if data.is_empty() {
        return Ok(());
    }

    let ss = sector_size as usize;
    let aligned_len = round_up(data.len(), ss);
    let mut buf = AlignedBuf::new(aligned_len, ss)?;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);
    // Remainder is already zero from alloc_zeroed.

    write_all(file, buf.as_slice())
}

pub(crate) fn write_at(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    // Concurrent-safe positioned write — Windows' equivalent of
    // POSIX `pwrite`. We pass the offset via an `OVERLAPPED`
    // struct rather than `SetFilePointerEx`-then-`WriteFile`,
    // because the latter mutates the per-fd cursor and is NOT
    // thread-safe across concurrent callers on the same fd.
    //
    // (0.8.0 R-1 tier-2 fix. Earlier versions used
    // SetFilePointerEx + WriteFile, which the journal substrate's
    // multi-thread concurrent-append benchmark surfaced as
    // anti-scaling: aggregate throughput went DOWN as thread
    // count went up because threads raced on the cursor.)
    //
    // For synchronous file handles (those NOT opened with
    // FILE_FLAG_OVERLAPPED — fsys's default), MSDN documents
    // that passing OVERLAPPED with the offset fields set causes
    // WriteFile to write at that exact offset synchronously.
    // The fd cursor *does* advance after the call, but two
    // threads each passing distinct offsets via OVERLAPPED do
    // not race on the cursor for the *write* itself.
    let handle = file.as_raw_handle() as HANDLE;

    let mut written_total: u32 = 0;
    while (written_total as usize) < data.len() {
        let remaining = data.len() - written_total as usize;
        // WriteFile takes a u32 length; cap at u32::MAX.
        let chunk_len: u32 = remaining.min(u32::MAX as usize) as u32;
        let chunk_offset = offset + written_total as u64;

        // Build the OVERLAPPED struct. Only the offset fields
        // need to be set; hEvent stays zero (we're synchronous).
        // Zeroing via std::mem::zeroed is sound — OVERLAPPED is
        // a plain old struct with no invalid bit patterns.
        // SAFETY: OVERLAPPED is repr(C), all-zero bit pattern
        // is a valid initial value per Windows API contract.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // `Anonymous` is a union {Anonymous: { Offset, OffsetHigh }, Pointer }.
        // Writing to a union variant is safe (only reading is
        // unsafe because the active variant might not match).
        overlapped.Anonymous.Anonymous.Offset = (chunk_offset & 0xFFFF_FFFF) as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (chunk_offset >> 32) as u32;

        let mut written: u32 = 0;
        let buf_ptr = data[written_total as usize..].as_ptr();
        // SAFETY: handle is valid; buf_ptr points to chunk_len
        // valid bytes; written is a valid out-pointer; overlapped
        // is a valid OVERLAPPED struct with offset fields set.
        let ok: BOOL =
            unsafe { WriteFile(handle, buf_ptr, chunk_len, &mut written, &mut overlapped) };
        if ok == FALSE {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        if written == 0 {
            return Err(Error::Io(std::io::Error::other(
                "WriteFile returned 0 bytes written in write_at",
            )));
        }
        written_total += written;
    }
    Ok(())
}

/// Sector-aligned positioned write for `FILE_FLAG_NO_BUFFERING` files.
///
/// **Pre-conditions** (caller-enforced):
/// - `data.as_ptr()` is sector-aligned.
/// - `data.len()` is a multiple of the sector size.
/// - `offset` is a multiple of the sector size.
///
/// Same `WriteFile` + `OVERLAPPED` path as [`write_at`]; the
/// alignment invariants come from the caller (the journal direct-mode
/// log buffer is allocated from `AlignedBuf` and flushed only at
/// sector boundaries).
pub(crate) fn write_at_direct(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    write_at(file, offset, data)
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

/// 0.9.5 — Punches a hole at `[offset, offset + len)` via
/// `DeviceIoControl(FSCTL_SET_ZERO_DATA)`.
///
/// Windows' `FSCTL_SET_ZERO_DATA` is the closest semantic match
/// to Linux `fallocate(PUNCH_HOLE)` / macOS `F_PUNCHHOLE`. On
/// NTFS sparse files the operation truly releases backing
/// blocks; on regular (non-sparse) NTFS files it zero-fills the
/// range without releasing storage — equivalent semantics from
/// the caller's perspective (reads return zeros after the call).
///
/// The IOCTL takes a `FILE_ZERO_DATA_INFORMATION` payload
/// (16 bytes: two `LARGE_INTEGER`s for the inclusive start +
/// exclusive end byte offsets of the range to zero).
pub(crate) fn punch_hole(file: &File, offset: u64, len: u64) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    if len == 0 {
        return Ok(());
    }

    /// `FILE_ZERO_DATA_INFORMATION` — start and end (exclusive)
    /// byte offsets of the range to zero. Both `LONGLONG`
    /// (i64) on Windows.
    #[repr(C)]
    struct FileZeroDataInformation {
        file_offset: i64,
        beyond_final_zero: i64,
    }
    /// `FSCTL_SET_ZERO_DATA` ioctl code.
    /// Equivalent C macro: `CTL_CODE(FILE_DEVICE_FILE_SYSTEM=0x09, 50,
    /// METHOD_BUFFERED=0, FILE_WRITE_DATA=2)` = 0x000980c8.
    const FSCTL_SET_ZERO_DATA: u32 = 0x0009_80c8;

    let payload = FileZeroDataInformation {
        file_offset: offset as i64,
        beyond_final_zero: offset.saturating_add(len) as i64,
    };
    let handle = file.as_raw_handle() as HANDLE;
    let mut bytes_returned: u32 = 0;
    // SAFETY: handle is owned by `file` for the duration of this
    // call. `payload` is a stack-allocated, properly-aligned
    // `FILE_ZERO_DATA_INFORMATION`. The ioctl reads exactly
    // `size_of::<FileZeroDataInformation>()` bytes; we pass the
    // matching size.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_ZERO_DATA,
            &payload as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<FileZeroDataInformation>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    if ok != FALSE {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

pub(crate) fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    // 0.9.6 — Try `FSCTL_DUPLICATE_EXTENTS_TO_FILE` for instant
    // copy-on-write reflinks on ReFS volumes. ReFS clones extents
    // metadata-only — a multi-GiB checkpoint clone drops from
    // seconds to microseconds.
    //
    // Hard requirements (kernel enforces; failure paths fall back):
    // - Both files MUST be on the same ReFS volume. NTFS / FAT /
    //   exFAT / network shares all return ERROR_INVALID_FUNCTION.
    // - Destination must already exist and be at least as large as
    //   the source range — we extend it via SetEndOfFile before
    //   issuing the ioctl.
    // - Both handles must be opened with `FILE_SHARE_DELETE`
    //   (omission causes ERROR_INVALID_PARAMETER).
    //
    // On any failure we fall back to `std::fs::copy` (which wraps
    // `CopyFileExW`) for full-byte-copy semantics. Correctness is
    // guaranteed regardless of which path runs.
    if let Ok(bytes) = try_reflink_refs(src, dst) {
        return Ok(bytes);
    }
    std::fs::copy(src, dst).map_err(Error::Io)
}

/// 0.9.6 — Attempts a ReFS `FSCTL_DUPLICATE_EXTENTS_TO_FILE` reflink
/// of `src` to `dst`. Returns the byte count cloned on success.
///
/// Returns `Err` on any of: source open failure, source-size query
/// failure, destination create/extend failure, FSCTL rejection
/// (non-ReFS volume, cross-volume copy, ineligible source range).
/// The caller falls back to a byte-copy on `Err`.
fn try_reflink_refs(src: &Path, dst: &Path) -> Result<u64> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FileEndOfFileInfo, GetFileSizeEx, SetFileInformationByHandle, FILE_END_OF_FILE_INFO,
        FILE_SHARE_DELETE,
    };
    use windows_sys::Win32::System::Ioctl::{
        DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let src_wide = to_wide(src);
    let dst_wide = to_wide(dst);

    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

    // Open source for read.
    // SAFETY: `src_wide` is a NUL-terminated UTF-16 path; flags are
    // valid; the returned handle either is INVALID_HANDLE_VALUE
    // (-1, error) or owned by us until we close it via
    // `File::from_raw_handle` Drop.
    let src_handle = unsafe {
        CreateFileW(
            src_wide.as_ptr(),
            GENERIC_READ,
            share,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut::<core::ffi::c_void>(),
        )
    };
    if src_handle.is_null() || src_handle == INVALID_HANDLE_VALUE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: src_handle is a valid open Windows file handle owned
    // by us; wrapping in File so Drop closes it cleanly even on
    // early return below.
    let src_file = unsafe { File::from_raw_handle(src_handle as RawHandle) };

    // Get source size.
    let mut src_size: i64 = 0;
    // SAFETY: src_handle is valid; GetFileSizeEx writes through the
    // out-pointer and returns 0/nonzero for failure/success.
    let ok: BOOL = unsafe { GetFileSizeEx(src_handle, &mut src_size) };
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    // Open destination for write — CREATE_NEW so we don't overwrite
    // an existing file silently. If dst exists, this fails and we
    // fall back cleanly (matching `std::fs::copy`'s overwrite
    // semantics via the fallback path).
    // SAFETY: dst_wide is NUL-term UTF-16; flags valid; handle
    // either error or owned by us.
    let dst_handle = unsafe {
        CreateFileW(
            dst_wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            share,
            std::ptr::null(),
            CREATE_NEW,
            0,
            std::ptr::null_mut::<core::ffi::c_void>(),
        )
    };
    if dst_handle.is_null() || dst_handle == INVALID_HANDLE_VALUE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: dst_handle is a valid open Windows file handle owned
    // by us; wrap in File so Drop closes it cleanly.
    let _dst_file = unsafe { File::from_raw_handle(dst_handle as RawHandle) };

    // Extend dst to src_size so the duplicate-extents call can map
    // into a valid dst range. FILE_END_OF_FILE_INFO uses an i64
    // EndOfFile value.
    let eof_info = FILE_END_OF_FILE_INFO {
        EndOfFile: src_size,
    };
    // SAFETY: dst_handle valid; eof_info is a stack-allocated
    // FILE_END_OF_FILE_INFO with a single i64 field; size argument
    // matches the struct size; SetFileInformationByHandle reads
    // through the pointer and returns 0/nonzero.
    let ok: BOOL = unsafe {
        SetFileInformationByHandle(
            dst_handle,
            FileEndOfFileInfo,
            &eof_info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<FILE_END_OF_FILE_INFO>() as u32,
        )
    };
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    // Empty source — nothing to duplicate. Truncated dst is the
    // correct result.
    if src_size == 0 {
        return Ok(0);
    }

    // Issue FSCTL_DUPLICATE_EXTENTS_TO_FILE. The src handle is
    // passed via the DUPLICATE_EXTENTS_DATA struct; the dst handle
    // is the DeviceIoControl target.
    let mut params = DUPLICATE_EXTENTS_DATA {
        FileHandle: src_handle as HANDLE,
        SourceFileOffset: 0,
        TargetFileOffset: 0,
        ByteCount: src_size,
    };
    let mut bytes_returned: u32 = 0;
    // SAFETY: dst_handle is the ioctl target (valid open handle);
    // params is a stack DUPLICATE_EXTENTS_DATA pointing at the
    // valid src_handle; sizes are accurate; the ioctl returns 0
    // (error) or nonzero (success).
    let ok: BOOL = unsafe {
        DeviceIoControl(
            dst_handle,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            &mut params as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut::<OVERLAPPED>(),
        )
    };
    if ok == FALSE {
        // Common failure codes the caller's fallback handles:
        // - ERROR_INVALID_FUNCTION (1) — not ReFS, kernel doesn't
        //   know this FSCTL.
        // - ERROR_INVALID_PARAMETER (87) — cross-volume, unaligned
        //   range, or other contract violation.
        // - ERROR_ACCESS_DENIED (5) — privilege / sharing.
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    // Suppress unused-variable lint on src_file — it exists only for
    // its Drop to close the source handle.
    let _ = &src_file;
    Ok(src_size as u64)
}

// ──────────────────────────────────────────────────────────────────────────────
// Probes
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Storage-engine primitives — preallocate + advise
// ──────────────────────────────────────────────────────────────────────────────

/// Windows preallocate via `SetFileInformationByHandle` with
/// `FileAllocationInfo` — the proper analog to Linux's
/// `fallocate(FALLOC_FL_KEEP_SIZE)`. Reserves NTFS extents
/// without changing the file's logical size (EOF). The journal's
/// reader doesn't see zero-filled tail bytes; the writer's
/// subsequent `WriteFile` calls land on pre-reserved extents
/// without per-write allocation jitter.
///
/// Note: `SetFileInformationByHandle(FileAllocationInfo)` requests
/// allocation; the actual disk blocks may still be lazily zeroed
/// by NTFS on first write. True physical preallocation (zero-
/// initialised blocks at preallocate time) requires
/// `SetFileValidData` which needs the `SE_MANAGE_VOLUME_NAME`
/// privilege. The current implementation is the best-available
/// non-privileged path.
pub(crate) fn preallocate(file: &File, offset: u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    use windows_sys::Win32::Storage::FileSystem::{
        FileAllocationInfo, GetFileSizeEx, SetFileInformationByHandle, FILE_ALLOCATION_INFO,
    };
    let handle = file.as_raw_handle() as HANDLE;

    // Compute target allocation size.
    let end = offset.saturating_add(len);
    let target = i64::try_from(end).map_err(|_| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preallocate target offset exceeds i64::MAX",
        ))
    })?;

    // Don't shrink — only grow allocation.
    let mut current: i64 = 0;
    // SAFETY: handle is valid; GetFileSizeEx writes the size to the out-pointer.
    let ok: BOOL = unsafe { GetFileSizeEx(handle, &mut current) };
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    if target <= current {
        return Ok(());
    }

    let info = FILE_ALLOCATION_INFO {
        AllocationSize: target,
    };
    // SAFETY: handle is valid; FileAllocationInfo expects a
    // FILE_ALLOCATION_INFO struct of size_of::<FILE_ALLOCATION_INFO>().
    let ok: BOOL = unsafe {
        SetFileInformationByHandle(
            handle,
            FileAllocationInfo,
            &info as *const _ as *const _,
            std::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    };
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Windows advise — best-effort no-op for runtime hints. Windows
/// lacks a per-range cache advisory API equivalent to
/// `posix_fadvise`. Sequential / Random hints CAN be applied at
/// file-open time via `FILE_FLAG_SEQUENTIAL_SCAN` /
/// `FILE_FLAG_RANDOM_ACCESS`, but only at open and only at the
/// whole-file granularity.
///
/// We accept the call and return `Ok(())` so cross-platform
/// callers don't need to `cfg`-gate. Future Windows-specific
/// improvements can wire in `PrefetchVirtualMemory` for
/// `WillNeed`.
pub(crate) fn advise(_file: &File, _offset: u64, _len: u64, _advice: crate::Advice) -> Result<()> {
    Ok(())
}

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
