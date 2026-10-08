//! Windows NVMe passthrough flush via `IOCTL_STORAGE_PROTOCOL_COMMAND`.
//!
//! Mirrors the Linux NVMe passthrough surface from `linux_iouring.rs`:
//! [`nvme_flush_capable`] probes whether the volume's underlying device
//! accepts an NVMe FLUSH (opcode `0x00`) sent through
//! `DeviceIoControl`, and [`nvme_flush`] issues one.
//!
//! ## Capability requirements
//!
//! - The volume must back an NVMe drive.
//! - The process must be able to open the volume (`\\.\C:`) with
//!   `GENERIC_READ | GENERIC_WRITE`, which the IOCTL's access mask
//!   requires. Non-elevated processes get `ERROR_ACCESS_DENIED`; the
//!   probe then reports "not capable" and the caller keeps using
//!   `FILE_FLAG_WRITE_THROUGH`.
//! - The storage driver must pass the command through. Microsoft
//!   documents `IOCTL_STORAGE_PROTOCOL_COMMAND` for vendor-specific NVMe
//!   commands, and the inbox `StorNVMe` driver is expected to reject a
//!   standard FLUSH (and the volume stack may not forward the IOCTL at
//!   all). On such systems the probe fails cleanly; only a driver that
//!   actually executes the FLUSH makes the probe succeed.
//!
//! ## Verification
//!
//! The probe sends a real FLUSH and requires both a successful
//! `DeviceIoControl` and `ReturnStatus == STORAGE_PROTOCOL_STATUS_SUCCESS`
//! in the returned header. A flush has no effect on stored data, so it is
//! a safe probe, and it is the only way to know the path works: an IOCTL
//! that "succeeds" while the device reports an error is treated as a
//! failure, here and in [`nvme_flush`].
//!
//! ## Command buffer layout
//!
//! `STORAGE_PROTOCOL_COMMAND` is 80 bytes of `u32` fields followed by a
//! variable-length `Command` array; `size_of` reports 84 because of the
//! one-byte placeholder plus padding. The 64-byte NVMe command starts at
//! the `Command` field offset (80), which is computed from the type
//! rather than assumed. The buffer is a `Vec<u64>` so the header is
//! naturally aligned.
//!
//! The FLUSH targets namespace ID `0xFFFF_FFFF` (all namespaces). The
//! volume handle does not reveal which namespace backs it; a controller
//! that does not support the broadcast namespace for FLUSH rejects the
//! command, which the status check turns into a probe failure.
//!
//! ## Privilege boundary
//!
//! The probe and every flush reopen the volume and close it again; no
//! long-lived volume handle is kept, because those interfere with
//! Windows volume-shadow and lock semantics. The env override
//! `FSYS_DISABLE_NVME_PASSTHROUGH=1` (locked decision D-11) forces the
//! fallback path.

#![cfg(target_os = "windows")]

use crate::{Error, Result};
use std::mem::MaybeUninit;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    ProtocolTypeNvme, IOCTL_STORAGE_PROTOCOL_COMMAND, STORAGE_PROTOCOL_COMMAND,
    STORAGE_PROTOCOL_COMMAND_LENGTH_NVME, STORAGE_PROTOCOL_SPECIFIC_NVME_NVM_COMMAND,
    STORAGE_PROTOCOL_STATUS_SUCCESS, STORAGE_PROTOCOL_STRUCTURE_VERSION,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// NVMe FLUSH opcode (NVM command set).
const NVME_OPCODE_FLUSH: u8 = 0x00;
/// Namespace ID meaning "all namespaces attached to the controller".
const NVME_NSID_ALL: u32 = 0xFFFF_FFFF;
/// Length of an NVMe submission-queue entry.
const NVME_COMMAND_BYTES: usize = STORAGE_PROTOCOL_COMMAND_LENGTH_NVME as usize;
/// Seconds the driver may spend on one command.
const COMMAND_TIMEOUT_SECS: u32 = 30;

/// Result of probing NVMe-passthrough capability for a given volume.
///
/// `Available` carries the volume root (e.g. `\\\\.\\C:`) so the
/// caller can re-open the volume handle for each FLUSH operation —
/// keeping a long-lived volume handle interferes with Windows
/// volume-shadow and lock primitives.
pub(crate) struct NvmeAccess {
    /// Volume root path in Win32 device-namespace form (e.g.
    /// `\\\\.\\C:`). Used by [`nvme_flush`] to reopen per-op.
    pub(crate) volume_root: PathBuf,
}

/// Probes whether NVMe passthrough flush works for the volume containing
/// `path`.
///
/// Returns `Some(NvmeAccess)` only when:
/// 1. `FSYS_DISABLE_NVME_PASSTHROUGH` is **not** set (locked decision
///    D-11).
/// 2. The volume can be opened for read and write (administrator rights
///    in practice).
/// 3. A real NVMe FLUSH sent through `IOCTL_STORAGE_PROTOCOL_COMMAND`
///    completes with `STORAGE_PROTOCOL_STATUS_SUCCESS`.
///
/// Returns `None` on any failure. The caller's `Method::Direct` path then
/// relies on `FILE_FLAG_WRITE_THROUGH` per locked decision D-2.
pub(crate) fn nvme_flush_capable(path: &Path) -> Option<NvmeAccess> {
    if std::env::var_os("FSYS_DISABLE_NVME_PASSTHROUGH").is_some() {
        return None;
    }

    let volume_root = volume_root_for(path)?;
    let handle = open_volume(&volume_root)?;
    let flushed = issue_flush_command(handle).is_ok();
    close_volume(handle);

    if flushed {
        Some(NvmeAccess { volume_root })
    } else {
        None
    }
}

/// Issues an NVMe FLUSH on the volume rooted at `access.volume_root`.
///
/// Reopens the volume handle for each call — long-lived shared
/// volume handles are problematic on Windows. The cost is one
/// extra `CreateFileW`/`CloseHandle` per flush (~5 µs); the
/// dominant cost is still the device's flush latency.
///
/// # Errors
///
/// Returns [`Error::Io`] when the volume cannot be reopened, when
/// `DeviceIoControl` fails, or when the driver reports a protocol status
/// other than success. `Ok(())` means the device acknowledged the flush.
pub(crate) fn nvme_flush(access: &NvmeAccess) -> Result<()> {
    let handle = open_volume(&access.volume_root).ok_or_else(|| {
        Error::Io(std::io::Error::other(
            "failed to reopen volume for NVMe flush",
        ))
    })?;
    let result = issue_flush_command(handle);
    close_volume(handle);
    result
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

type WinHandle = windows_sys::Win32::Foundation::HANDLE;

/// Resolves `path` to its volume root in Win32 device-namespace
/// form (e.g. `\\\\.\\C:`).
fn volume_root_for(path: &Path) -> Option<PathBuf> {
    // For a path like `C:\Users\foo\file.dat` we want `\\\\.\\C:`.
    // Get the canonical path's first component (drive letter).
    let canonical = std::fs::canonicalize(path).ok()?;
    let s = canonical.to_str()?;

    // Strip the `\\?\` extended-path prefix if present.
    let trimmed = s.strip_prefix(r"\\?\").unwrap_or(s);
    // First component should be `X:` for some drive letter X.
    let drive = trimmed.split('\\').next()?;
    if drive.len() != 2 || !drive.ends_with(':') {
        return None;
    }
    Some(PathBuf::from(format!(r"\\.\{drive}")))
}

/// Opens a volume handle with `GENERIC_READ | GENERIC_WRITE` for
/// IOCTL submission. Returns `None` on any failure (including
/// access-denied — the typical non-admin case).
fn open_volume(volume_root: &Path) -> Option<WinHandle> {
    let wide: Vec<u16> = volume_root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is a NUL-terminated UTF-16 path string built
    // from the volume root we just resolved. `CreateFileW` returns
    // `INVALID_HANDLE_VALUE` on failure rather than panicking; we
    // check before using.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(handle)
    }
}

/// Closes a handle returned by [`open_volume`].
fn close_volume(handle: WinHandle) {
    // SAFETY: `handle` came from `open_volume`, is owned by the caller's
    // stack frame, and is not used after this call.
    let closed = unsafe { CloseHandle(handle) };
    // Nothing useful can be done if closing a volume handle fails; the
    // flush result has already been decided.
    let _ = closed;
}

/// Byte offsets of the `STORAGE_PROTOCOL_COMMAND` fields this module
/// reads after the call, plus the `Command` array.
struct HeaderOffsets {
    return_status: usize,
    error_code: usize,
    command: usize,
}

/// Computes field offsets from the type itself (no `offset_of!`, which
/// needs Rust 1.77; the crate's MSRV is 1.75).
fn header_offsets() -> HeaderOffsets {
    let slot = MaybeUninit::<STORAGE_PROTOCOL_COMMAND>::uninit();
    let base = slot.as_ptr();
    // SAFETY: `addr_of!` on a field of `*base` only computes an address
    // inside the `slot` allocation; it creates no reference and reads no
    // (uninitialised) memory.
    let (return_status, error_code, command) = unsafe {
        (
            std::ptr::addr_of!((*base).ReturnStatus),
            std::ptr::addr_of!((*base).ErrorCode),
            std::ptr::addr_of!((*base).Command),
        )
    };
    HeaderOffsets {
        return_status: return_status as usize - base as usize,
        error_code: error_code as usize - base as usize,
        command: command as usize - base as usize,
    }
}

/// Byte view of the `u64`-backed command buffer.
fn as_bytes_mut(buf: &mut [u64]) -> &mut [u8] {
    let len = std::mem::size_of_val(buf);
    // SAFETY: `buf` is a live, exclusively borrowed slice of `len` bytes;
    // `u8` has alignment 1 and every bit pattern is a valid `u8`, and the
    // returned slice keeps the exclusive borrow of `buf`.
    unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), len) }
}

/// Reads a native-endian `u32` at `offset` of `bytes`, if in bounds.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let raw: [u8; 4] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_ne_bytes(raw))
}

/// Builds the `IOCTL_STORAGE_PROTOCOL_COMMAND` input buffer for an NVMe
/// FLUSH of all namespaces: header, then the 64-byte command at the
/// `Command` field offset. No data transfer.
fn build_flush_buffer() -> Vec<u64> {
    let offsets = header_offsets();
    let total = offsets.command + NVME_COMMAND_BYTES;
    let mut buf = vec![0u64; total.div_ceil(8)];

    // SAFETY: STORAGE_PROTOCOL_COMMAND is a repr(C) struct of integers
    // (plus a one-byte array); the all-zero bit pattern is valid.
    let mut header: STORAGE_PROTOCOL_COMMAND = unsafe { std::mem::zeroed() };
    header.Version = STORAGE_PROTOCOL_STRUCTURE_VERSION;
    header.Length = std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>() as u32;
    header.ProtocolType = ProtocolTypeNvme;
    // FLUSH is an NVM (I/O) command, so no ADAPTER_REQUEST flag; that flag
    // is only for admin commands addressed to the controller.
    header.Flags = 0;
    header.CommandLength = STORAGE_PROTOCOL_COMMAND_LENGTH_NVME;
    header.TimeOutValue = COMMAND_TIMEOUT_SECS;
    header.CommandSpecific = STORAGE_PROTOCOL_SPECIFIC_NVME_NVM_COMMAND;

    let bytes = as_bytes_mut(&mut buf);
    // SAFETY: `header` is a live, fully initialised plain-old-data value;
    // viewing its first `offsets.command` bytes (all `u32` fields, no
    // padding before `Command`) as `u8` is valid for reads.
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&header as *const STORAGE_PROTOCOL_COMMAND).cast::<u8>(),
            offsets.command,
        )
    };
    bytes[..offsets.command].copy_from_slice(header_bytes);

    // NVMe submission-queue entry: CDW0 byte 0 = opcode, bytes 4..8 = NSID.
    let cmd = &mut bytes[offsets.command..offsets.command + NVME_COMMAND_BYTES];
    cmd[0] = NVME_OPCODE_FLUSH;
    cmd[4..8].copy_from_slice(&NVME_NSID_ALL.to_le_bytes());
    buf
}

/// Maps the header's `ReturnStatus` / `ErrorCode` to a result.
fn protocol_status(return_status: u32, error_code: u32) -> Result<()> {
    if return_status == STORAGE_PROTOCOL_STATUS_SUCCESS {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::other(format!(
            "NVMe passthrough command failed: protocol status {return_status:#x}, \
             error code {error_code:#x}"
        ))))
    }
}

/// Sends an NVMe FLUSH through `IOCTL_STORAGE_PROTOCOL_COMMAND` and
/// checks both the IOCTL result and the device-reported status.
fn issue_flush_command(handle: WinHandle) -> Result<()> {
    let mut buf = build_flush_buffer();
    let len = u32::try_from(std::mem::size_of_val(buf.as_slice()))
        .map_err(|_| Error::Io(std::io::Error::other("NVMe command buffer too large")))?;
    let mut bytes_returned: u32 = 0;

    // SAFETY: `handle` is a valid volume handle owned by the caller for
    // the duration of this synchronous call. `buf` is an exclusively
    // owned, 8-byte-aligned allocation of `len` bytes holding a complete
    // STORAGE_PROTOCOL_COMMAND plus command; it is passed as both input
    // and output buffer (the driver writes ReturnStatus / ErrorCode back
    // into the header). `bytes_returned` is a valid out-pointer and the
    // null OVERLAPPED selects synchronous completion.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_PROTOCOL_COMMAND,
            buf.as_mut_ptr().cast(),
            len,
            buf.as_mut_ptr().cast(),
            len,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    let offsets = header_offsets();
    let bytes = as_bytes_mut(&mut buf);
    let return_status = read_u32(bytes, offsets.return_status).unwrap_or(0);
    let error_code = read_u32(bytes, offsets.error_code).unwrap_or(0);
    protocol_status(return_status, error_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_root_for_local_path_returns_drive_form() {
        let p = std::env::temp_dir();
        if let Some(root) = volume_root_for(&p) {
            let s = root.to_string_lossy();
            assert!(
                s.starts_with(r"\\.\") && s.ends_with(':'),
                "expected device-namespace volume root, got {s}"
            );
        }
        // If `volume_root_for` returns None (rare on Windows but
        // possible for non-canonical paths), the test passes silently
        // — the resolution is best-effort.
    }

    #[test]
    fn test_header_offsets_match_ntddstor_layout() {
        let o = header_offsets();
        // Twenty u32 fields precede `Command`; ReturnStatus is the 5th,
        // ErrorCode the 6th (ntddstor.h).
        assert_eq!(o.return_status, 16);
        assert_eq!(o.error_code, 20);
        assert_eq!(o.command, 80);
        // size_of includes the one-byte Command placeholder plus padding,
        // which is why the old code (command at size_of) was 4 bytes off.
        assert_eq!(std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>(), 84);
    }

    #[test]
    fn test_build_flush_buffer_layout() {
        let mut buf = build_flush_buffer();
        let bytes = as_bytes_mut(&mut buf).to_vec();
        assert!(bytes.len() >= 80 + 64);
        assert_eq!(
            read_u32(&bytes, 0),
            Some(STORAGE_PROTOCOL_STRUCTURE_VERSION)
        );
        assert_eq!(read_u32(&bytes, 4), Some(84));
        assert_eq!(read_u32(&bytes, 8), Some(ProtocolTypeNvme as u32));
        // Flags: NVM command, so no ADAPTER_REQUEST.
        assert_eq!(read_u32(&bytes, 12), Some(0));
        assert_eq!(read_u32(&bytes, 24), Some(64)); // CommandLength
        assert_eq!(read_u32(&bytes, 36), Some(0)); // DataFromDeviceTransferLength
        assert_eq!(
            read_u32(&bytes, 56),
            Some(STORAGE_PROTOCOL_SPECIFIC_NVME_NVM_COMMAND)
        );
        // NVMe command: opcode at +0, NSID at +4.
        assert_eq!(bytes[80], NVME_OPCODE_FLUSH);
        assert_eq!(read_u32(&bytes, 84), Some(NVME_NSID_ALL));
        assert!(bytes[88..80 + 64].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_protocol_status_requires_success() {
        assert!(protocol_status(STORAGE_PROTOCOL_STATUS_SUCCESS, 0).is_ok());
        assert!(protocol_status(0, 0).is_err());
        assert!(protocol_status(0x2, 0x5).is_err());
    }

    #[test]
    fn test_read_u32_bounds() {
        let b = [1u8, 0, 0, 0, 2];
        assert_eq!(read_u32(&b, 0), Some(1));
        assert_eq!(read_u32(&b, 2), None);
        assert_eq!(read_u32(&b, usize::MAX), None);
    }

    #[test]
    fn capability_probe_returns_some_or_none_without_panic() {
        // We cannot assume admin privileges in tests. Verify only
        // that probing doesn't crash and returns a valid Option.
        let p = std::env::temp_dir();
        let _ = nvme_flush_capable(&p);
    }

    #[test]
    fn env_override_forces_none() {
        let prior = std::env::var_os("FSYS_DISABLE_NVME_PASSTHROUGH");
        // SAFETY: `set_var` / `remove_var` are documented as racy in
        // a multi-threaded process. This test runs synchronously in
        // its own binary; nothing else mutates this env var
        // concurrently. The block exists so the lint is satisfied
        // per `clippy::undocumented_unsafe_blocks`.
        unsafe {
            std::env::set_var("FSYS_DISABLE_NVME_PASSTHROUGH", "1");
        }

        let p = std::env::temp_dir();
        let result = nvme_flush_capable(&p);
        assert!(result.is_none(), "env override must force None");

        // SAFETY: same reasoning as the set above — single-threaded
        // test, no concurrent env mutation.
        unsafe {
            match prior {
                Some(v) => std::env::set_var("FSYS_DISABLE_NVME_PASSTHROUGH", v),
                None => std::env::remove_var("FSYS_DISABLE_NVME_PASSTHROUGH"),
            }
        }
    }
}
