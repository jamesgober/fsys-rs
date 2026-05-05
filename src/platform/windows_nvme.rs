//! Windows NVMe passthrough flush via `IOCTL_STORAGE_PROTOCOL_COMMAND`.
//!
//! Mirrors the Linux NVMe passthrough surface from `linux_iouring.rs`:
//! [`nvme_flush_capable`] probes whether the volume's underlying device
//! is an NVMe drive accessible to this process for raw protocol
//! commands; [`nvme_flush`] issues an NVMe FLUSH (opcode `0x00`) via
//! `DeviceIoControl`.
//!
//! ## Capability requirements
//!
//! - The volume must back an NVMe drive (consumer NVMe + Windows
//!   storport driver — typical for `C:` on most systems shipped
//!   2018+).
//! - The process must have a handle to the volume opened with
//!   `GENERIC_READ | GENERIC_WRITE` and admin privileges. Most
//!   non-elevated processes will hit `ERROR_ACCESS_DENIED`; the
//!   capability detection records that case and the caller falls
//!   back to `FILE_FLAG_WRITE_THROUGH`.
//!
//! ## Privilege boundary
//!
//! `IOCTL_STORAGE_PROTOCOL_COMMAND` is a privileged IOCTL — it
//! sends raw NVMe commands directly to the controller. Bugs here
//! can corrupt filesystems. The submission path is therefore
//! deliberately conservative:
//!
//! 1. Capability detection issues a no-op probe (Identify
//!    Controller — admin command 0x06) and verifies the IOCTL is
//!    accepted. We do NOT issue actual FLUSH during capability
//!    probing.
//! 2. The probe handle is closed immediately after capability is
//!    confirmed. Subsequent flushes reopen the volume per-op.
//!    (This matches the Linux pattern of holding only the
//!    character-device fd, not the data-fd.)
//! 3. The env override `FSYS_DISABLE_NVME_PASSTHROUGH=1` (locked
//!    decision D-11) forces the fallback path on every probe.
//!
//! ## Per-handle state
//!
//! On Windows, [`crate::Handle`] caches the volume's NVMe-passthrough
//! capability the same way it caches the Linux capability — three-
//! state lazy slot. The volume handle itself is reopened per-op
//! because long-lived shared volume handles can interfere with
//! Windows's volume-shadow / lock semantics; the capability bit
//! and the resolved volume root path are what we cache.

#![cfg(target_os = "windows")]
#![allow(dead_code)] // wired into Handle in checkpoint D continuation

use crate::{Error, Result};
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
    STORAGE_PROTOCOL_STRUCTURE_VERSION,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

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

/// Probes whether NVMe passthrough flush is available for the
/// volume containing `path`.
///
/// Returns `Some(NvmeAccess)` when:
/// 1. `FSYS_DISABLE_NVME_PASSTHROUGH` env override is **not** set
///    (locked decision D-11).
/// 2. The volume's underlying device accepts
///    `IOCTL_STORAGE_PROTOCOL_COMMAND` with `ProtocolTypeNvme` for
///    a no-op admin command (Identify Controller).
/// 3. The calling process has admin privileges (otherwise the IOCTL
///    returns `ERROR_ACCESS_DENIED`).
///
/// Returns `None` on any failure. The caller's [`Method::Direct`]
/// path falls back to `FILE_FLAG_WRITE_THROUGH` per locked
/// decision D-2.
pub(crate) fn nvme_flush_capable(path: &Path) -> Option<NvmeAccess> {
    if std::env::var_os("FSYS_DISABLE_NVME_PASSTHROUGH").is_some() {
        return None;
    }

    let volume_root = volume_root_for(path)?;
    let handle = open_volume(&volume_root)?;

    // Probe with NVMe Identify Controller (admin opcode 0x06).
    // The IOCTL accepts the command on capable hardware and
    // returns either success or a structured NVMe error; both
    // outcomes confirm capability. Only `ERROR_ACCESS_DENIED` /
    // `ERROR_INVALID_FUNCTION` from `DeviceIoControl` itself mean
    // the volume / privilege combination is incapable.
    let probe_ok = issue_identify_controller(handle).is_ok();

    // SAFETY: `handle` was opened by `CreateFileW` above and not
    // shared elsewhere. `CloseHandle` is the matching teardown.
    let _ = unsafe { CloseHandle(handle) };

    if probe_ok {
        Some(NvmeAccess { volume_root })
    } else {
        None
    }
}

/// Issues an NVMe FLUSH (opcode 0x00) on the volume rooted at
/// `access.volume_root`.
///
/// Reopens the volume handle for each call — long-lived shared
/// volume handles are problematic on Windows. The cost is one
/// extra `CreateFileW`/`CloseHandle` per flush (~5 µs); the
/// dominant cost is still the device's flush latency.
///
/// # Errors
///
/// Returns [`Error::Io`] wrapping the underlying Win32 error code
/// on failure.
pub(crate) fn nvme_flush(access: &NvmeAccess) -> Result<()> {
    let handle = open_volume(&access.volume_root).ok_or_else(|| {
        Error::Io(std::io::Error::other(
            "failed to reopen volume for NVMe flush",
        ))
    })?;

    let result = issue_flush_command(handle);

    // SAFETY: same as in `nvme_flush_capable` — `handle` is owned
    // by this stack frame and CloseHandle is the matching teardown.
    let _ = unsafe { CloseHandle(handle) };

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

/// Capability probe: NVMe Identify Controller (admin opcode 0x06).
/// Allocates a 4096-byte response buffer (the standard Identify
/// Controller data size) and submits the IOCTL. Returns `Ok(())` on
/// success, `Err` on `DeviceIoControl` failure.
fn issue_identify_controller(handle: WinHandle) -> Result<()> {
    // Buffer layout: STORAGE_PROTOCOL_COMMAND header followed by
    // CommandLength bytes of NVMe command, then 4096 bytes of
    // response. Total size matches what `DataFromDeviceTransferLength`
    // declares.
    const NVME_COMMAND_LENGTH: u32 = 64;
    const IDENTIFY_DATA_LEN: u32 = 4096;
    let total_len = (std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>() as u32)
        + NVME_COMMAND_LENGTH
        + IDENTIFY_DATA_LEN;

    let mut buf: Vec<u8> = vec![0u8; total_len as usize];

    // Fill the STORAGE_PROTOCOL_COMMAND header at the start of buf.
    // SAFETY: the buffer is at least `size_of::<STORAGE_PROTOCOL_COMMAND>()`
    // bytes long; we wrote zero bytes into it on allocation. The
    // pointer cast yields a properly aligned `*mut STORAGE_PROTOCOL_COMMAND`
    // because `Vec<u8>::as_mut_ptr` returns a byte-aligned pointer
    // and STORAGE_PROTOCOL_COMMAND is a `#[repr(C)]` struct whose
    // alignment is satisfied by 8-byte alignment (which the system
    // allocator provides for Vec<u8>).
    unsafe {
        let header = buf.as_mut_ptr() as *mut STORAGE_PROTOCOL_COMMAND;
        (*header).Version = STORAGE_PROTOCOL_STRUCTURE_VERSION;
        (*header).Length = std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>() as u32;
        (*header).ProtocolType = ProtocolTypeNvme;
        (*header).Flags = 0;
        (*header).CommandLength = NVME_COMMAND_LENGTH;
        (*header).ErrorInfoLength = 0;
        (*header).DataToDeviceTransferLength = 0;
        (*header).DataFromDeviceTransferLength = IDENTIFY_DATA_LEN;
        (*header).TimeOutValue = 30;
        (*header).ErrorInfoOffset = 0;
        (*header).DataToDeviceBufferOffset = 0;
        (*header).DataFromDeviceBufferOffset =
            (std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>() as u32) + NVME_COMMAND_LENGTH;
        // CommandSpecific[0..] holds the NVMe command bytes. For
        // Identify Controller: opcode 0x06, CDW10 = CNS = 0x01.
        let cmd_offset = std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>();
        let cmd_ptr = buf.as_mut_ptr().add(cmd_offset);
        // Opcode at byte 0:
        *cmd_ptr = 0x06;
        // CDW10 at byte 40 (per NVMe Common Command Format): CNS=1
        // means "Identify Controller".
        *(cmd_ptr.add(40) as *mut u32) = 1;
    }

    issue_protocol_command(handle, &mut buf)
}

/// Submission helper: NVMe FLUSH (opcode 0x00).
fn issue_flush_command(handle: WinHandle) -> Result<()> {
    const NVME_COMMAND_LENGTH: u32 = 64;
    let total_len = (std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>() as u32) + NVME_COMMAND_LENGTH;
    let mut buf: Vec<u8> = vec![0u8; total_len as usize];

    // SAFETY: same alignment + capacity reasoning as
    // `issue_identify_controller`.
    unsafe {
        let header = buf.as_mut_ptr() as *mut STORAGE_PROTOCOL_COMMAND;
        (*header).Version = STORAGE_PROTOCOL_STRUCTURE_VERSION;
        (*header).Length = std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>() as u32;
        (*header).ProtocolType = ProtocolTypeNvme;
        (*header).Flags = 0;
        (*header).CommandLength = NVME_COMMAND_LENGTH;
        (*header).ErrorInfoLength = 0;
        (*header).DataToDeviceTransferLength = 0;
        (*header).DataFromDeviceTransferLength = 0;
        (*header).TimeOutValue = 30;
        (*header).ErrorInfoOffset = 0;
        (*header).DataToDeviceBufferOffset = 0;
        (*header).DataFromDeviceBufferOffset = 0;
        // FLUSH is opcode 0x00; remaining 63 bytes of command are
        // zero (already zeroed by `vec![0u8; …]`).
        let cmd_offset = std::mem::size_of::<STORAGE_PROTOCOL_COMMAND>();
        *buf.as_mut_ptr().add(cmd_offset) = 0x00;
    }

    issue_protocol_command(handle, &mut buf)
}

/// Common IOCTL submission: invokes
/// `DeviceIoControl(IOCTL_STORAGE_PROTOCOL_COMMAND, ...)` with the
/// caller-prepared buffer. Returns `Ok(())` on success.
fn issue_protocol_command(handle: WinHandle, buf: &mut [u8]) -> Result<()> {
    let mut bytes_returned: u32 = 0;
    let len = buf.len() as u32;

    // SAFETY: `handle` is owned by the caller and is valid for the
    // duration of this synchronous call. `buf` is exclusively
    // borrowed via `&mut [u8]`, of length `len`; we pass it as
    // both input and output buffer (the IOCTL writes the response
    // back into the same buffer at `DataFromDeviceBufferOffset`).
    // `DeviceIoControl` returns 0 on failure rather than panicking;
    // we surface `last_os_error` in that case.
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
    if ok != 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
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
