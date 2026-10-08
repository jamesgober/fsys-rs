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
//! - **Copy:** `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS block clone) when
//!   the source volume supports block cloning, otherwise (or on any clone
//!   failure) `std::fs::copy`, which wraps `CopyFileExW`.

#![cfg(target_os = "windows")]

use crate::{Error, Result};
use std::fs::File;
use std::io::Read;
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

/// Largest byte count handed to a single `ReadFile` / `WriteFile` call.
///
/// The Win32 calls take a `u32` length, so payloads of 4 GiB or more must
/// be split. 2 GiB is a multiple of every power-of-two sector size, which
/// keeps each chunk legal on `FILE_FLAG_NO_BUFFERING` handles (`u32::MAX`
/// itself is not a sector multiple and would be rejected).
const MAX_IO_CHUNK: usize = 1 << 31;

pub(crate) fn write_all(file: &File, data: &[u8]) -> Result<()> {
    write_all_chunked(file, data, MAX_IO_CHUNK)
}

/// Cursor-based `WriteFile` loop, at most `max_chunk` bytes per call.
///
/// A call that reports success with zero bytes written is an
/// `ErrorKind::WriteZero` error rather than an endless retry.
fn write_all_chunked(file: &File, data: &[u8], max_chunk: usize) -> Result<()> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut offset = 0usize;

    while offset < data.len() {
        let chunk_len = io_chunk_len(data.len() - offset, max_chunk);
        let mut written = 0u32;
        // SAFETY: handle is valid for the duration of the call;
        // `data[offset..]` has at least `chunk_len` readable bytes;
        // `written` is a valid out-pointer; a null OVERLAPPED selects
        // synchronous cursor-based IO.
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
        if written == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "WriteFile reported success with 0 bytes written",
            )));
        }
        offset += written as usize;
    }
    Ok(())
}

/// Length of the next IO chunk: `remaining` capped at `max_chunk` and at
/// `u32::MAX` (the Win32 length type).
fn io_chunk_len(remaining: usize, max_chunk: usize) -> u32 {
    let capped = remaining.min(max_chunk);
    u32::try_from(capped).unwrap_or(u32::MAX)
}

/// Builds a synchronous-IO `OVERLAPPED` that carries `offset`.
fn overlapped_at(offset: u64) -> OVERLAPPED {
    // SAFETY: OVERLAPPED is a repr(C) plain-old-data struct; the all-zero
    // bit pattern (no event, zero offset) is its documented initial value.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // Writing a union field is safe; only reads need `unsafe`.
    overlapped.Anonymous.Anonymous.Offset = (offset & 0xFFFF_FFFF) as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    overlapped
}

pub(crate) fn write_all_direct(file: &File, data: &[u8], sector_size: u32) -> Result<()> {
    use super::AlignedBuf;

    // Empty input — no-op. See linux.rs::write_all_direct for the
    // rationale (AlignedBuf::new rejects size=0).
    if data.is_empty() {
        return Ok(());
    }

    let ss = sector_size as usize;
    let aligned_len = checked_round_up(data.len(), ss)?;
    let mut buf = AlignedBuf::new(aligned_len, ss)?;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);
    // Remainder is already zero from alloc_zeroed.

    // MAX_IO_CHUNK is a sector multiple, so every chunk stays legal on a
    // FILE_FLAG_NO_BUFFERING handle.
    write_all_chunked(file, buf.as_slice(), MAX_IO_CHUNK)
}

/// Rounds `n` up to a multiple of the power-of-two `align`, failing
/// instead of overflowing.
fn checked_round_up(n: usize, align: usize) -> Result<usize> {
    if !align.is_power_of_two() {
        return Err(Error::AlignmentRequired {
            detail: "sector size is not a power of two",
        });
    }
    n.checked_add(align - 1)
        .map(|v| v & !(align - 1))
        .ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Direct IO length overflows when rounded up to the sector size",
            ))
        })
}

pub(crate) fn write_at(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    write_at_chunked(file, offset, data, MAX_IO_CHUNK)
}

/// Positioned `WriteFile` loop, at most `max_chunk` bytes per call.
fn write_at_chunked(file: &File, offset: u64, data: &[u8], max_chunk: usize) -> Result<()> {
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

    let mut written_total = 0usize;
    while written_total < data.len() {
        let chunk_len = io_chunk_len(data.len() - written_total, max_chunk);
        let chunk_offset = offset_plus(offset, written_total)?;
        let mut overlapped = overlapped_at(chunk_offset);

        let mut written: u32 = 0;
        // SAFETY: handle is valid; `data[written_total..]` has at least
        // `chunk_len` readable bytes; `written` is a valid out-pointer;
        // `overlapped` is a live OVERLAPPED carrying the offset.
        let ok: BOOL = unsafe {
            WriteFile(
                handle,
                data[written_total..].as_ptr(),
                chunk_len,
                &mut written,
                &mut overlapped,
            )
        };
        if ok == FALSE {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        if written == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "WriteFile returned 0 bytes written in write_at",
            )));
        }
        written_total += written as usize;
    }
    Ok(())
}

/// `base + delta` as a file offset, or an `InvalidInput` error when the
/// sum does not fit in the signed 64-bit range Windows accepts.
fn offset_plus(base: u64, delta: usize) -> Result<u64> {
    base.checked_add(delta as u64)
        .filter(|v| i64::try_from(*v).is_ok())
        .ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file offset overflow",
            ))
        })
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
    read_all_direct_chunked(file, file_size, sector_size, MAX_IO_CHUNK)
}

/// Positioned `ReadFile` loop for `FILE_FLAG_NO_BUFFERING` handles.
///
/// Reads from offset 0 into one sector-aligned buffer until `file_size`
/// bytes have arrived, at most `max_chunk` bytes per call (rounded down
/// to a sector multiple so every request stays legal). End of file before
/// `file_size` bytes is an `ErrorKind::UnexpectedEof` error rather than a
/// silently short result.
fn read_all_direct_chunked(
    file: &File,
    file_size: u64,
    sector_size: u32,
    max_chunk: usize,
) -> Result<Vec<u8>> {
    use super::AlignedBuf;

    if file_size == 0 {
        return Ok(Vec::new());
    }

    let size = usize::try_from(file_size).map_err(|_| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file is larger than the address space",
        ))
    })?;
    let ss = sector_size as usize;
    let aligned_len = checked_round_up(size, ss)?;
    // Largest sector multiple not above `max_chunk`, and at least one
    // sector so the loop always makes progress.
    let chunk_cap = (max_chunk & !(ss - 1)).max(ss);
    let mut buf = AlignedBuf::new(aligned_len, ss)?;

    let handle = file.as_raw_handle() as HANDLE;
    let mut total = 0usize;
    while total < size {
        let chunk_len = io_chunk_len(aligned_len - total, chunk_cap);
        let mut overlapped = overlapped_at(total as u64);
        let mut got: u32 = 0;
        // SAFETY: handle is valid for the call; `buf` owns `aligned_len`
        // writable bytes and `total + chunk_len <= aligned_len`, so the
        // kernel writes only inside the allocation; `total` is a sector
        // multiple here (every earlier chunk was a full sector multiple,
        // otherwise the loop already ended at EOF), keeping the pointer
        // and the offset sector-aligned; `got` and `overlapped` are live.
        let ok: BOOL = unsafe {
            ReadFile(
                handle,
                buf.as_mut_slice()[total..].as_mut_ptr().cast(),
                chunk_len,
                &mut got,
                &mut overlapped,
            )
        };
        if ok == FALSE {
            let err = std::io::Error::last_os_error();
            // ERROR_HANDLE_EOF: a positioned read starting at or past EOF.
            if err.raw_os_error() != Some(38) {
                return Err(Error::Io(err));
            }
            got = 0;
        }
        if got == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file ended before the expected size during a Direct IO read",
            )));
        }
        total += got as usize;
    }

    Ok(buf.as_slice()[..size].to_vec())
}

pub(crate) fn read_range(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    // `FileExt::seek_read` passes the offset in an OVERLAPPED struct, so
    // concurrent callers on one handle each read their own range. As with
    // `write_at`, Windows still moves the handle's cursor after the call;
    // nothing in fsys reads through the cursor of a handle it range-reads.
    // A read starting at or past EOF comes back as `Ok(0)`.
    use std::os::windows::fs::FileExt;
    super::read_range_with(file, offset, len, |buf, pos| {
        // ReadFile takes a u32 length; a short read just loops.
        let cap = buf.len().min(MAX_IO_CHUNK);
        file.seek_read(&mut buf[..cap], pos)
    })
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

/// Barrier-grade sync for the journal's `SyncMode::Barrier`.
///
/// A handle opened with `FILE_FLAG_WRITE_THROUGH` has already made each
/// write durable before `WriteFile` returned, so there is nothing left to
/// flush and the call returns `Ok(())` without touching the device. Every
/// other handle (including the default buffered journal, which is opened
/// through `std::fs::OpenOptions`, and the direct journal's buffered
/// fallback when the volume rejects `FILE_FLAG_NO_BUFFERING`) gets a full
/// `FlushFileBuffers`, identical to [`sync_full`].
///
/// The write-through check reads the handle's `FILE_MODE_INFORMATION`
/// through `NtQueryInformationFile`. If that query fails for any reason
/// the function assumes the handle is not write-through and flushes.
///
/// Measured on a Windows 11 NVMe laptop (4 KiB writes): write-through
/// write ~165 us/op, the same write plus `FlushFileBuffers` ~540 us/op,
/// the mode query adds no measurable time.
pub(crate) fn sync_barrier(file: &File) -> Result<()> {
    if handle_is_write_through(file) {
        return Ok(());
    }
    sync_full(file)
}

/// `FILE_INFORMATION_CLASS::FileModeInformation` (ntifs.h).
const FILE_MODE_INFORMATION_CLASS: u32 = 16;
/// `FILE_WRITE_THROUGH` bit of `FILE_MODE_INFORMATION::Mode` (ntifs.h).
/// Set when the handle was opened with `FILE_FLAG_WRITE_THROUGH`.
const FILE_MODE_WRITE_THROUGH: u32 = 0x0000_0002;

/// `IO_STATUS_BLOCK` (wdm.h): a pointer-sized `Status`/`Pointer` union
/// followed by a `ULONG_PTR Information`. Declared locally so no extra
/// `windows-sys` feature is needed.
#[repr(C)]
struct IoStatusBlock {
    status_or_pointer: *mut std::ffi::c_void,
    information: usize,
}

// Plain `extern` (not `unsafe extern`): MSRV is 1.75 and `unsafe extern`
// blocks need 1.82. Calls through the block are still `unsafe`.
#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationFile(
        file_handle: HANDLE,
        io_status_block: *mut IoStatusBlock,
        file_information: *mut std::ffi::c_void,
        length: u32,
        file_information_class: u32,
    ) -> i32;
}

/// Returns `true` when `file`'s handle was opened with
/// `FILE_FLAG_WRITE_THROUGH`. Returns `false` when it was not, or when
/// the query fails (callers treat `false` as "flush to be safe").
fn handle_is_write_through(file: &File) -> bool {
    let mut iosb = IoStatusBlock {
        status_or_pointer: std::ptr::null_mut(),
        information: 0,
    };
    // FILE_MODE_INFORMATION is a single ULONG.
    let mut mode: u32 = 0;
    // SAFETY: the handle is owned by `file` and stays open for the call.
    // `iosb` and `mode` are live, writable, correctly sized stack values;
    // `length` is exactly `size_of::<u32>()`, the size of
    // FILE_MODE_INFORMATION, so the kernel writes at most 4 bytes into
    // `mode`. The function returns an NTSTATUS and has no other effects.
    let status = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut iosb,
            (&mut mode as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
            FILE_MODE_INFORMATION_CLASS,
        )
    };
    // NTSTATUS >= 0 is success (STATUS_SUCCESS and informational codes).
    status >= 0 && (mode & FILE_MODE_WRITE_THROUGH) != 0
}

// ──────────────────────────────────────────────────────────────────────────────
// Rename and copy
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    let from_wide = to_wide(from);
    let to_wide = to_wide(to);

    // MOVEFILE_REPLACE_EXISTING: replace `to` if it exists.
    // MOVEFILE_WRITE_THROUGH: per the MoveFileExW documentation this only
    // waits for the flush when the move is carried out as copy + delete
    // (a cross-volume move). A same-volume rename returns once NTFS has
    // logged it, not once the log is on stable media; callers that need
    // the new name to survive a crash follow up with `sync_parent_dir`.
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

/// Flushes the directory that holds `path` so a preceding rename into it
/// is durable.
///
/// Opens the directory with `FILE_FLAG_BACKUP_SEMANTICS` (required for
/// directory handles) and `FILE_WRITE_DATA` access, the minimum
/// `FlushFileBuffers` accepts on a directory; no administrator rights or
/// backup privilege are needed. File systems that cannot flush a
/// directory handle report `ERROR_INVALID_FUNCTION`,
/// `ERROR_NOT_SUPPORTED` or `ERROR_INVALID_PARAMETER`; those are treated
/// as success (the pre-1.1.1 behaviour on every volume). Any other open
/// or flush error is returned.
pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_WRITE_DATA,
    };

    let wide = to_wide(super::parent_or_current_dir(path));
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
    // call; the flags are valid CreateFileW arguments; the result is
    // checked against INVALID_HANDLE_VALUE before use.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_WRITE_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `handle` is a valid directory handle we own; wrapping it in
    // a File closes it on every return path.
    let dir = unsafe { File::from_raw_handle(handle as RawHandle) };
    match sync_full(&dir) {
        Err(Error::Io(e)) if directory_flush_unsupported(e.raw_os_error()) => Ok(()),
        other => other,
    }
}

/// `true` for the Win32 errors a file system returns when it cannot flush
/// a directory handle: ERROR_INVALID_FUNCTION (1), ERROR_NOT_SUPPORTED
/// (50), ERROR_INVALID_PARAMETER (87).
fn directory_flush_unsupported(code: Option<i32>) -> bool {
    matches!(code, Some(1) | Some(50) | Some(87))
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
    // Requirements (kernel enforces; failure paths fall back):
    // - Both files on the same volume, and that volume supports block
    //   cloning (`FILE_SUPPORTS_BLOCK_REFCOUNTING`, ReFS). The source
    //   volume is checked before anything is created, so NTFS / FAT /
    //   network copies go straight to the byte copy.
    // - Clone ranges begin and end on cluster boundaries; the last
    //   partial cluster may be cloned whole because it ends at EOF.
    // - The destination is extended to the source size before the
    //   ioctl and a sparse source needs a sparse destination.
    // - Each ioctl clones less than 4 GiB.
    //
    // On any failure we fall back to `std::fs::copy` (which wraps
    // `CopyFileExW`) for full-byte-copy semantics. A destination that
    // the reflink attempt created is removed first, so a failed fallback
    // never leaves an extended but empty file behind.
    if let Ok(bytes) = try_reflink_refs(src, dst) {
        return Ok(bytes);
    }
    std::fs::copy(src, dst).map_err(Error::Io)
}

/// `FILE_SUPPORTS_BLOCK_REFCOUNTING` file-system flag (winnt.h): the
/// volume can clone extents with `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.
const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;

/// 0.9.6 — Attempts a ReFS `FSCTL_DUPLICATE_EXTENTS_TO_FILE` reflink
/// of `src` to `dst`. Returns the byte count cloned on success.
///
/// Returns `Err` on any of: source open failure, a source volume without
/// block cloning, destination create failure (including `dst` already
/// existing), or any failure while sizing or cloning into the new
/// destination. In the last case the destination this function created
/// is deleted before returning. The caller falls back to a byte-copy on
/// `Err`.
fn try_reflink_refs(src: &Path, dst: &Path) -> Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;

    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let src_file = open_with_share(src, GENERIC_READ, share, OPEN_EXISTING)?;
    if !volume_supports_block_cloning(&src_file) {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "source volume does not support block cloning",
        )));
    }

    // CREATE_NEW so an existing destination is never touched here; the
    // byte-copy fallback keeps `std::fs::copy`'s overwrite semantics.
    let dst_file = open_with_share(dst, GENERIC_READ | GENERIC_WRITE, share, CREATE_NEW)?;
    let result = clone_into_new_file(&src_file, &dst_file);
    if result.is_err() {
        drop(dst_file);
        // `dst` did not exist before CREATE_NEW above, so it holds nothing
        // of the caller's. Removing it lets the fallback start clean. If
        // the removal itself fails, the fallback `std::fs::copy` still
        // truncates and overwrites the file, so the error is not useful.
        let _removed = std::fs::remove_file(dst);
    }
    result
}

/// Clones all of `src` into the freshly created, empty `dst`.
fn clone_into_new_file(src: &File, dst: &File) -> Result<u64> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FileEndOfFileInfo, SetFileInformationByHandle, FILE_ATTRIBUTE_SPARSE_FILE,
        FILE_END_OF_FILE_INFO,
    };
    use windows_sys::Win32::System::Ioctl::{
        DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_SET_SPARSE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let src_meta = src.metadata().map_err(Error::Io)?;
    let src_size = src_meta.len();
    let src_size_i64 = i64::try_from(src_size).map_err(|_| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source size exceeds i64::MAX",
        ))
    })?;
    if src_size == 0 {
        return Ok(0);
    }
    let cluster = cluster_size(src)?;

    if src_meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        let mut bytes_returned: u32 = 0;
        // SAFETY: `dst` is a valid handle opened for write; FSCTL_SET_SPARSE
        // with no input buffer marks the file sparse; `bytes_returned` is a
        // valid out-pointer and the call is synchronous.
        let ok: BOOL = unsafe {
            DeviceIoControl(
                dst.as_raw_handle() as HANDLE,
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };
        if ok == FALSE {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
    }

    // The destination region must lie inside its EOF, so size it to the
    // source first. The last clone range is rounded up to a whole cluster,
    // which ReFS accepts because it ends at the source's EOF.
    let eof_info = FILE_END_OF_FILE_INFO {
        EndOfFile: src_size_i64,
    };
    // SAFETY: `dst` is a valid handle opened for write; `eof_info` is a
    // live FILE_END_OF_FILE_INFO and the size argument matches it.
    let ok: BOOL = unsafe {
        SetFileInformationByHandle(
            dst.as_raw_handle() as HANDLE,
            FileEndOfFileInfo,
            (&eof_info as *const FILE_END_OF_FILE_INFO).cast(),
            std::mem::size_of::<FILE_END_OF_FILE_INFO>() as u32,
        )
    };
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    for (offset, byte_count) in clone_ranges(src_size, cluster) {
        let mut params = DUPLICATE_EXTENTS_DATA {
            FileHandle: src.as_raw_handle() as HANDLE,
            SourceFileOffset: offset as i64,
            TargetFileOffset: offset as i64,
            ByteCount: byte_count as i64,
        };
        let mut bytes_returned: u32 = 0;
        // SAFETY: `dst` is the ioctl target (valid handle opened for
        // write); `params` is a live DUPLICATE_EXTENTS_DATA naming the
        // valid source handle, with offsets and count below i64::MAX
        // (bounded by `src_size_i64` rounded up to one cluster); the size
        // argument matches the struct; the call is synchronous.
        let ok: BOOL = unsafe {
            DeviceIoControl(
                dst.as_raw_handle() as HANDLE,
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                (&mut params as *mut DUPLICATE_EXTENTS_DATA).cast(),
                std::mem::size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                std::ptr::null_mut(),
                0,
                &mut bytes_returned,
                std::ptr::null_mut::<OVERLAPPED>(),
            )
        };
        if ok == FALSE {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
    }
    Ok(src_size)
}

/// Splits `[0, size)` into `(offset, byte_count)` clone requests: each
/// starts on a cluster boundary, covers less than 4 GiB, and is a whole
/// number of clusters (the last one is rounded up past `size`).
fn clone_ranges(size: u64, cluster: u64) -> Vec<(u64, u64)> {
    // Largest whole-cluster count strictly below 4 GiB.
    let max_chunk = ((1u64 << 32) - 1) / cluster * cluster;
    let mut out = Vec::new();
    let mut offset = 0u64;
    while offset < size {
        let remaining = size - offset;
        let take = remaining.min(max_chunk);
        let rounded = take.div_ceil(cluster) * cluster;
        out.push((offset, rounded));
        offset += take;
    }
    out
}

/// Opens `path` with `CreateFileW` and wraps the handle in a `File`.
fn open_with_share(path: &Path, access: u32, share: u32, disposition: u32) -> Result<File> {
    let wide = to_wide(path);
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
    // call; the access, share and disposition values are valid
    // CreateFileW arguments; the result is checked before use.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            share,
            std::ptr::null(),
            disposition,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `handle` is a valid open handle owned by us; `File` closes
    // it on drop.
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

/// `true` when the volume holding `file` advertises block cloning.
fn volume_supports_block_cloning(file: &File) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
    let mut fs_flags: u32 = 0;
    // SAFETY: the handle is owned by `file` for the call; every optional
    // buffer is null with a zero length, and `fs_flags` is a valid
    // out-pointer.
    let ok: BOOL = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut fs_flags,
            std::ptr::null_mut(),
            0,
        )
    };
    ok != FALSE && fs_flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0
}

/// Cluster size of the ReFS volume holding `file`, from
/// `FSCTL_GET_INTEGRITY_INFORMATION`.
fn cluster_size(file: &File) -> Result<u64> {
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_GET_INTEGRITY_INFORMATION, FSCTL_GET_INTEGRITY_INFORMATION_BUFFER,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    // SAFETY: the buffer is plain old data; all-zero is a valid value.
    let mut info: FSCTL_GET_INTEGRITY_INFORMATION_BUFFER = unsafe { std::mem::zeroed() };
    let mut bytes_returned: u32 = 0;
    // SAFETY: the handle is owned by `file` for the call; `info` is a live
    // output buffer whose exact size is passed; the call is synchronous.
    let ok: BOOL = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_GET_INTEGRITY_INFORMATION,
            std::ptr::null(),
            0,
            (&mut info as *mut FSCTL_GET_INTEGRITY_INFORMATION_BUFFER).cast(),
            std::mem::size_of::<FSCTL_GET_INTEGRITY_INFORMATION_BUFFER>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    let cluster = u64::from(info.ClusterSizeInBytes);
    if cluster == 0 || !cluster.is_power_of_two() {
        return Err(Error::Io(std::io::Error::other(
            "volume reported an invalid cluster size",
        )));
    }
    Ok(cluster)
}

// ──────────────────────────────────────────────────────────────────────────────
// Probes
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Storage-engine primitives — preallocate + advise
// ──────────────────────────────────────────────────────────────────────────────

/// Windows preallocate via `SetFileInformationByHandle` with
/// `FileAllocationInfo` — the analog of Linux's
/// `fallocate(FALLOC_FL_KEEP_SIZE)`. Reserves NTFS clusters for
/// `[0, offset + len)` without changing the file's logical size (EOF),
/// so later `WriteFile` calls land on reserved space without
/// per-write allocation.
///
/// The request only ever grows the reservation: when the file's current
/// `AllocationSize` (from `GetFileInformationByHandleEx(FileStandardInfo)`)
/// already covers `offset + len`, the call does nothing. Setting a smaller
/// allocation would release clusters reserved by an earlier, larger
/// `preallocate`, and setting it below EOF would truncate the file.
///
/// `SetFileValidData` (which also skips NTFS's lazy zeroing but needs the
/// `SE_MANAGE_VOLUME_NAME` privilege) is not used.
pub(crate) fn preallocate(file: &File, offset: u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    use windows_sys::Win32::Storage::FileSystem::{
        FileAllocationInfo, SetFileInformationByHandle, FILE_ALLOCATION_INFO,
    };
    let handle = file.as_raw_handle() as HANDLE;

    // Compute target allocation size.
    let end = offset.checked_add(len).ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preallocate range overflows u64",
        ))
    })?;
    let target = i64::try_from(end).map_err(|_| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preallocate target offset exceeds i64::MAX",
        ))
    })?;

    // Only grow the reservation, never shrink it.
    if target <= allocation_size(file)? {
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

/// Returns the bytes currently allocated to `file` on disk
/// (`FILE_STANDARD_INFO::AllocationSize`).
fn allocation_size(file: &File) -> Result<i64> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_STANDARD_INFO,
    };
    // SAFETY: FILE_STANDARD_INFO is plain old data (integers and byte
    // flags); the all-zero bit pattern is valid.
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is owned by `file` for the call; `info` is a live
    // FILE_STANDARD_INFO and the length passed is exactly its size, so the
    // kernel writes only inside it.
    let ok: BOOL = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileStandardInfo,
            (&mut info as *mut FILE_STANDARD_INFO).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok == FALSE {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(info.AllocationSize)
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
    fn test_handle_is_write_through_detects_flag() {
        let path = tmp_path("wt_flag");
        let _g = TmpFile(path.clone());
        let (f, direct) = open_write_new(&path, true).expect("open direct");
        // Direct opens use NO_BUFFERING | WRITE_THROUGH; the 87 fallback
        // reopens without either flag.
        assert_eq!(handle_is_write_through(&f), direct);
    }

    #[test]
    fn test_handle_is_write_through_false_for_buffered_handle() {
        let path = tmp_path("wt_plain");
        let _g = TmpFile(path.clone());
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open");
        assert!(!handle_is_write_through(&f));
    }

    #[test]
    fn test_sync_barrier_flushes_buffered_handle() {
        let path = tmp_path("barrier");
        let _g = TmpFile(path.clone());
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open");
        write_at(&f, 0, b"barrier").expect("write");
        sync_barrier(&f).expect("barrier on buffered handle");
        // A read-only handle cannot be flushed (FlushFileBuffers needs
        // GENERIC_WRITE); getting the error proves the buffered path
        // really calls FlushFileBuffers instead of returning early.
        let (ro, _) = open_read(&path, false).expect("open ro");
        assert!(sync_barrier(&ro).is_err());
    }

    #[test]
    fn test_sync_parent_dir_flushes_real_directory() {
        let path = tmp_path("dirsync");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"x").expect("seed");
        sync_parent_dir(&path).expect("flush temp dir");
    }

    #[test]
    fn test_directory_flush_unsupported_classification() {
        assert!(directory_flush_unsupported(Some(1)));
        assert!(directory_flush_unsupported(Some(50)));
        assert!(directory_flush_unsupported(Some(87)));
        assert!(!directory_flush_unsupported(Some(5)));
        assert!(!directory_flush_unsupported(None));
    }

    #[test]
    fn test_preallocate_never_shrinks_reservation() {
        let path = tmp_path("prealloc");
        let _g = TmpFile(path.clone());
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open");
        preallocate(&f, 0, 1 << 20).expect("1 MiB");
        let big = allocation_size(&f).expect("alloc");
        assert!(big >= 1 << 20, "allocation {big}");
        // A smaller follow-up request used to compare against EOF (0)
        // and shrink the reservation to 512 KiB.
        preallocate(&f, 0, 512 << 10).expect("512 KiB");
        assert_eq!(allocation_size(&f).expect("alloc"), big);
        // EOF is untouched throughout.
        assert_eq!(f.metadata().expect("meta").len(), 0);
        // Growing past the reservation still works.
        preallocate(&f, 1 << 20, 1 << 20).expect("grow");
        assert!(allocation_size(&f).expect("alloc") >= 2 << 20);
    }

    #[test]
    fn test_preallocate_rejects_overflowing_range() {
        let path = tmp_path("prealloc_ovf");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        assert!(preallocate(&f, u64::MAX, 2).is_err());
        assert!(preallocate(&f, u64::MAX / 2 + 1, 1).is_err());
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
    fn test_write_all_chunked_splits_large_payload() {
        let path = tmp_path("chunked_all");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        // 7-byte chunks force ~1430 WriteFile calls.
        write_all_chunked(&f, &data, 7).expect("write");
        drop(f);
        assert_eq!(std::fs::read(&path).expect("read"), data);
    }

    #[test]
    fn test_write_at_chunked_splits_and_offsets_each_chunk() {
        let path = tmp_path("chunked_at");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, vec![b'.'; 64]).expect("seed");
        let f = open_write_at(&path).expect("open");
        write_at_chunked(&f, 10, b"abcdefghijklmnopqrstuvwxyz", 5).expect("write");
        drop(f);
        let got = std::fs::read(&path).expect("read");
        assert_eq!(&got[..10], &[b'.'; 10]);
        assert_eq!(&got[10..36], b"abcdefghijklmnopqrstuvwxyz");
        assert_eq!(&got[36..], &[b'.'; 28]);
    }

    #[test]
    fn test_write_at_rejects_offset_overflow() {
        let path = tmp_path("ovf");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"x").expect("seed");
        let f = open_write_at(&path).expect("open");
        let err = write_at(&f, u64::MAX - 1, b"abc").expect_err("must fail");
        assert!(matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput));
    }

    #[test]
    fn test_io_chunk_len_caps_at_u32_and_max_chunk() {
        assert_eq!(io_chunk_len(10, 4), 4);
        assert_eq!(io_chunk_len(3, 4), 3);
        assert_eq!(io_chunk_len(usize::MAX, usize::MAX), u32::MAX);
        assert_eq!(io_chunk_len(usize::MAX, MAX_IO_CHUNK), 1 << 31);
        // The production cap is a multiple of every sector size up to 2 GiB.
        for shift in 9..=31 {
            assert_eq!(MAX_IO_CHUNK % (1usize << shift), 0);
        }
    }

    #[test]
    fn test_checked_round_up_rejects_overflow_and_bad_align() {
        assert_eq!(checked_round_up(1, 512).expect("ok"), 512);
        assert_eq!(checked_round_up(512, 512).expect("ok"), 512);
        assert!(checked_round_up(usize::MAX, 512).is_err());
        assert!(checked_round_up(10, 3).is_err());
    }

    /// Writes `len` patterned bytes with a buffered handle and returns
    /// the payload.
    fn seed_pattern(path: &Path, len: usize) -> Vec<u8> {
        let data: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
        std::fs::write(path, &data).expect("seed");
        data
    }

    #[test]
    fn test_read_all_direct_chunked_reads_every_chunk() {
        let path = tmp_path("rad_chunks");
        let _g = TmpFile(path.clone());
        let ss = probe_sector_size(&path);
        let len = ss as usize * 5 + 123;
        let data = seed_pattern(&path, len);
        let (f, direct) = open_read(&path, true).expect("open");
        // One sector per ReadFile call: six calls, the last one short.
        let got = read_all_direct_chunked(&f, len as u64, ss, ss as usize).expect("read");
        assert_eq!(got, data, "direct={direct}");
    }

    #[test]
    fn test_read_all_direct_chunked_rounds_cap_down_to_sector() {
        let path = tmp_path("rad_cap");
        let _g = TmpFile(path.clone());
        let ss = probe_sector_size(&path);
        let len = ss as usize * 3;
        let data = seed_pattern(&path, len);
        let (f, _) = open_read(&path, true).expect("open");
        // A cap that is not a sector multiple must still produce legal
        // NO_BUFFERING requests.
        let got = read_all_direct_chunked(&f, len as u64, ss, ss as usize + 7).expect("read");
        assert_eq!(got, data);
    }

    #[test]
    fn test_read_all_direct_errors_when_file_shorter_than_expected() {
        let path = tmp_path("rad_short");
        let _g = TmpFile(path.clone());
        let ss = probe_sector_size(&path);
        let len = ss as usize * 2;
        let _data = seed_pattern(&path, len);
        let (f, _) = open_read(&path, true).expect("open");
        let err = read_all_direct_chunked(&f, (len * 2) as u64, ss, ss as usize)
            .expect_err("premature EOF must error");
        assert!(matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof));
    }

    #[test]
    fn test_read_all_direct_empty_file() {
        let path = tmp_path("rad_empty");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"").expect("seed");
        let (f, _) = open_read(&path, true).expect("open");
        assert!(read_all_direct(&f, 0, 512).expect("read").is_empty());
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
    fn test_clone_ranges_cluster_aligned_and_below_4_gib() {
        assert_eq!(clone_ranges(5000, 4096), vec![(0, 8192)]);
        assert_eq!(clone_ranges(4096, 4096), vec![(0, 4096)]);
        assert_eq!(clone_ranges(1, 65536), vec![(0, 65536)]);
        let big = (5u64 << 30) + 123;
        let ranges = clone_ranges(big, 65536);
        assert_eq!(ranges.len(), 2);
        for (offset, count) in &ranges {
            assert_eq!(offset % 65536, 0);
            assert_eq!(count % 65536, 0);
            assert!(*count < 1u64 << 32);
        }
        let (last_off, last_count) = ranges[1];
        assert!(last_off + last_count >= big);
        assert!(last_off + last_count - big < 65536);
        assert!(clone_ranges(0, 4096).is_empty());
    }

    #[test]
    fn test_copy_file_unaligned_size_on_non_refs_volume() {
        // The temp volume is NTFS here: the reflink attempt must bail out
        // before creating `dst`, and the byte copy must be exact.
        let src = tmp_path("cp_odd_src");
        let dst = tmp_path("cp_odd_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        std::fs::write(&src, &data).expect("write");
        assert_eq!(copy_file(&src, &dst).expect("copy"), 5000);
        assert_eq!(std::fs::read(&dst).expect("read"), data);
    }

    #[test]
    fn test_copy_file_missing_source_leaves_no_destination() {
        let src = tmp_path("cp_missing_src");
        let dst = tmp_path("cp_missing_dst");
        let _gd = TmpFile(dst.clone());
        assert!(copy_file(&src, &dst).is_err());
        assert!(!dst.exists());
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
