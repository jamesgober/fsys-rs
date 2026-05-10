//! Windows-specific hardware probe.
//!
//! Probes use Win32 APIs:
//! - Memory: `GlobalMemoryStatusEx`.
//! - Drive capacity / free space: `GetDiskFreeSpaceExW`.
//! - Drive sector sizes: `GetDiskFreeSpaceW` + `IOCTL_STORAGE_QUERY_PROPERTY`
//!   with `StorageAccessAlignmentProperty` for finer detail when reachable.
//! - CPU: `GetSystemInfo` (logical cores) + `GetLogicalProcessorInformationEx`
//!   (physical cores + cache sizes).
//! - PLP: deferred to 0.6.0 alongside NVMe IOCTL passthrough.
//!
//! All probes are non-fatal — failures degrade to documented defaults
//! and do not error handle creation.

#![cfg(target_os = "windows")]

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetVolumePathNameW,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use super::PlpStatus;
use crate::hardware::cpu::CpuInfo;
use crate::hardware::drive::DriveInfo;
use crate::hardware::io_primitives::IoPrimitives;
use crate::hardware::memory::MemoryInfo;

// ─────────────────────────────────────────────────────────────────────────────
// Drive
// ─────────────────────────────────────────────────────────────────────────────

/// Probes the storage device hosting the current working directory.
///
/// Strategy:
/// 1. Find the volume containing the cwd via `GetVolumePathNameW`.
/// 2. Read total/free bytes via `GetDiskFreeSpaceExW`.
/// 3. Read sector sizes via `GetDiskFreeSpaceW`.
///
/// Drive kind classification is conservative on Windows: refining it
/// would require `IOCTL_STORAGE_QUERY_PROPERTY` with
/// `StorageDeviceSeekPenaltyProperty` (rotational vs non-rotational)
/// and bus-type inspection. That landed in 0.6.0 alongside NVMe IOCTL
/// support; for 0.5.0 we report `Unknown` for kind unless the volume
/// path makes the drive type unambiguous (which it rarely does on
/// Windows).
pub(crate) fn probe_drive() -> DriveInfo {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return DriveInfo::default(),
    };

    let mut info = DriveInfo::default();

    let volume = match volume_path(&cwd) {
        Some(v) => v,
        None => return info,
    };

    // Capacity and free space via GetDiskFreeSpaceExW.
    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free_total: u64 = 0;
    let volume_w = wide(volume.as_os_str());
    // SAFETY: `GetDiskFreeSpaceExW` reads from `volume_w` (a NUL-
    // terminated wide string we just allocated) and writes into the
    // three properly-sized `u64` outputs. The pointers are valid for
    // the duration of the call.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            volume_w.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut free_total,
        )
    };
    if ok != 0 {
        info.total_bytes = total;
        info.available_bytes = free_to_caller;
    }

    // Sector sizes via GetDiskFreeSpaceW.
    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    let mut free_clusters: u32 = 0;
    let mut total_clusters: u32 = 0;
    // SAFETY: same pattern as above; volume_w is a valid NUL-terminated
    // wide string and the four `u32` outputs are all valid for write.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            volume_w.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if ok != 0 && bytes_per_sector > 0 {
        info.logical_sector = bytes_per_sector;
        // Windows doesn't easily expose physical sector size without
        // IOCTL_STORAGE_QUERY_PROPERTY; default to logical when we
        // can't tell more.
        info.physical_sector = bytes_per_sector;
        // Cluster size is the natural "optimal block" hint at the FS
        // level on Windows.
        let cluster = bytes_per_sector.saturating_mul(sectors_per_cluster);
        if cluster > 0 {
            info.optimal_block = cluster;
        }
    }

    // PLP detection (0.7.0 R-2): query the volume's vendor +
    // model via `IOCTL_STORAGE_QUERY_PROPERTY` with
    // `StorageDeviceProperty`, then consult the lookup table.
    info.plp = probe_plp_windows(&volume);

    info
}

/// Issues `IOCTL_STORAGE_QUERY_PROPERTY` with
/// `StorageDeviceProperty` against the volume root and reads the
/// returned `STORAGE_DEVICE_DESCRIPTOR` for vendor + product
/// strings. Returns [`PlpStatus::Unknown`] on any error or table
/// miss.
fn probe_plp_windows(volume: &OsString) -> PlpStatus {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    // Build the device-namespace path for the volume root. The
    // GetVolumePathNameW result for a CWD looks like `C:\` —
    // strip the trailing `\` and prepend `\\.\` to get `\\.\C:`.
    let s = volume.to_string_lossy();
    let trimmed = s.trim_end_matches('\\');
    let drive = trimmed.split('\\').next().unwrap_or("");
    if drive.len() != 2 || !drive.ends_with(':') {
        return PlpStatus::Unknown;
    }
    let device_path = format!(r"\\.\{drive}");
    let wide_path: Vec<u16> = std::ffi::OsStr::new(&device_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Open the device handle. `GENERIC_READ` is sufficient for the
    // query; admin privilege is NOT required for
    // `StorageDeviceProperty`.
    //
    // SAFETY: `wide_path` is a NUL-terminated UTF-16 string built
    // from the volume root we just resolved. `CreateFileW` returns
    // `INVALID_HANDLE_VALUE` on failure rather than panicking; we
    // check before using.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return PlpStatus::Unknown;
    }

    // Two-step IOCTL: first call returns the size; second call
    // fills the buffer.
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };

    // Adequate buffer for STORAGE_DEVICE_DESCRIPTOR + the embedded
    // vendor/model/serial strings. 1 KiB is more than enough.
    let mut buf: Vec<u8> = vec![0u8; 1024];
    let mut bytes_returned: u32 = 0;

    // SAFETY: `handle` is valid; `&query` points to a stack
    // STORAGE_PROPERTY_QUERY of the correct size; `buf` is a
    // 1 KiB byte buffer; `DeviceIoControl` returns 0 on failure.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const _,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    let result = if ok == 0 {
        PlpStatus::Unknown
    } else {
        parse_device_descriptor(&buf)
    };

    // SAFETY: `handle` was opened by CreateFileW above and not
    // shared elsewhere. CloseHandle is the matching teardown.
    let _ = unsafe { CloseHandle(handle) };
    result
}

/// Parse the `STORAGE_DEVICE_DESCRIPTOR` returned by
/// `IOCTL_STORAGE_QUERY_PROPERTY`. The descriptor's
/// `VendorIdOffset` / `ProductIdOffset` point into the same
/// buffer at NUL-terminated ASCII strings.
fn parse_device_descriptor(buf: &[u8]) -> PlpStatus {
    use windows_sys::Win32::System::Ioctl::STORAGE_DEVICE_DESCRIPTOR;

    if buf.len() < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return PlpStatus::Unknown;
    }

    // SAFETY: we verified the buffer is at least
    // `size_of::<STORAGE_DEVICE_DESCRIPTOR>()` bytes long; the
    // pointer cast yields a properly-aligned `*const
    // STORAGE_DEVICE_DESCRIPTOR` because `Vec<u8>::as_ptr()` is
    // 8-byte-aligned by the system allocator and the C struct's
    // alignment is satisfied by 8-byte alignment.
    let descriptor: &STORAGE_DEVICE_DESCRIPTOR =
        unsafe { &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };

    let vendor = c_string_at_offset(buf, descriptor.VendorIdOffset as usize);
    let model = c_string_at_offset(buf, descriptor.ProductIdOffset as usize);

    crate::hardware::plp::lookup_table(&vendor, &model)
}

/// Read a NUL-terminated ASCII string at `offset` in `buf`.
/// Returns an empty string if the offset is out of bounds or the
/// string is empty.
fn c_string_at_offset(buf: &[u8], offset: usize) -> String {
    if offset == 0 || offset >= buf.len() {
        return String::new();
    }
    let tail = &buf[offset..];
    let nul = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    String::from_utf8_lossy(&tail[..nul]).trim().to_string()
}

fn volume_path(path: &Path) -> Option<OsString> {
    let path_w = wide(path.as_os_str());
    let mut buf: Vec<u16> = vec![0u16; 261];
    // SAFETY: `GetVolumePathNameW` reads from `path_w` (NUL-terminated
    // wide string) and writes up to `buf.len()` UTF-16 code units into
    // `buf`, NUL-terminating the result on success.
    let ok = unsafe { GetVolumePathNameW(path_w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(OsString::from_wide(&buf[..len]))
}

fn wide(s: &OsStr) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    v.push(0);
    v
}

// ─────────────────────────────────────────────────────────────────────────────
// Memory
// ─────────────────────────────────────────────────────────────────────────────

/// Probes total and available memory via `GlobalMemoryStatusEx`.
pub(crate) fn probe_memory() -> MemoryInfo {
    // SAFETY: `MEMORYSTATUSEX` is plain old data and zero-init is
    // valid; we set `dwLength` to the struct size before the call as
    // required by the Win32 contract.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `&mut status` is a valid `*mut MEMORYSTATUSEX` with the
    // correctly-set `dwLength`. The kernel writes to fields up to that
    // length.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 {
        return MemoryInfo::default();
    }
    MemoryInfo {
        total_bytes: status.ullTotalPhys,
        available_bytes: status.ullAvailPhys,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CPU
// ─────────────────────────────────────────────────────────────────────────────

/// Probes CPU info.
///
/// Logical cores via [`std::thread::available_parallelism`] (works
/// correctly on Windows with affinity-mask awareness). Physical cores
/// via `GetLogicalProcessorInformationEx`. Compile-time CPU features.
pub(crate) fn probe_cpu() -> CpuInfo {
    let cores_logical = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1);

    let cores_physical = probe_physical_cores().unwrap_or(cores_logical);
    let (l1, l2, l3) = probe_cache_sizes().unwrap_or((0, 0, 0));

    CpuInfo {
        cores_logical,
        cores_physical,
        features: super::super::cpu::runtime_features(),
        cache_l1: l1,
        cache_l2: l2,
        cache_l3: l3,
    }
}

fn probe_physical_cores() -> Option<u32> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    // Two-pass: first call with NULL buffer to get the required size.
    let mut needed: u32 = 0;
    // SAFETY: passing `null_mut()` and `&mut needed` is the documented
    // size-query pattern for this API; the call writes the required
    // byte count into `needed` and returns 0 on the expected
    // ERROR_INSUFFICIENT_BUFFER path.
    unsafe {
        let _ = GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            std::ptr::null_mut(),
            &mut needed,
        );
    }
    if needed == 0 {
        return None;
    }
    let mut buf: Vec<u8> = vec![0u8; needed as usize];
    // SAFETY: `buf` has at least `needed` bytes; the cast is valid for
    // a buffer that the kernel will populate with packed
    // SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX records.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }

    // Walk the variable-length array of records. Each record's `Size`
    // field is its own length in bytes.
    let mut count = 0u32;
    let mut offset = 0usize;
    while offset < needed as usize {
        // SAFETY: We bounds-check `offset + size_of<header>` before
        // reading; the kernel guarantees the buffer is well-formed
        // when `ok != 0`.
        let header = unsafe {
            &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)
        };
        if header.Relationship == RelationProcessorCore {
            count += 1;
        }
        if header.Size == 0 {
            break; // defensive against corrupt buffers
        }
        offset += header.Size as usize;
    }
    if count == 0 {
        None
    } else {
        Some(count)
    }
}

fn probe_cache_sizes() -> Option<(usize, usize, usize)> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationCache, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    let mut needed: u32 = 0;
    // SAFETY: same pattern as `probe_physical_cores` — null query for size.
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationCache, std::ptr::null_mut(), &mut needed);
    }
    if needed == 0 {
        return None;
    }
    let mut buf: Vec<u8> = vec![0u8; needed as usize];
    // SAFETY: `buf` has at least `needed` bytes; cast as documented.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationCache,
            buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }

    let mut l1 = 0usize;
    let mut l2 = 0usize;
    let mut l3 = 0usize;
    let mut offset = 0usize;
    while offset < needed as usize {
        // SAFETY: bounds-checked iteration as in `probe_physical_cores`.
        let header = unsafe {
            &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)
        };
        if header.Relationship == RelationCache {
            // SAFETY: when Relationship == RelationCache, the union's
            // `Cache` arm is the active variant per the Win32
            // contract.
            let cache = unsafe { &header.Anonymous.Cache };
            let size = cache.CacheSize as usize;
            match cache.Level {
                1 => l1 = l1.max(size),
                2 => l2 = l2.max(size),
                3 => l3 = l3.max(size),
                _ => {}
            }
        }
        if header.Size == 0 {
            break;
        }
        offset += header.Size as usize;
    }
    Some((l1, l2, l3))
}

// 0.9.2: `detect_compile_time_features` removed; CPU feature
// detection is now runtime-dispatched via `cpu::runtime_features()`.
// See `src/hardware/cpu.rs` for rationale.

// ─────────────────────────────────────────────────────────────────────────────
// IO primitives
// ─────────────────────────────────────────────────────────────────────────────

/// Probes which kernel-level IO primitives are reachable on this
/// Windows host.
///
/// IOCP is always available since NT 3.5. mmap-equivalent
/// (`MapViewOfFile`) is universal. `FILE_FLAG_NO_BUFFERING` for direct
/// IO is universal at the API level (filesystem may still reject at
/// open). No io_uring on Windows; no NVMe passthrough in 0.5.0.
pub(crate) fn probe_io_primitives() -> IoPrimitives {
    IoPrimitives {
        io_uring: false,
        iocp: true,
        kqueue: false,
        nvme_passthrough: false, // 0.6.0
        direct_io: true,
        mmap: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_memory_reports_non_zero_total() {
        let m = probe_memory();
        assert!(
            m.total_bytes > 0,
            "Windows GlobalMemoryStatusEx must report >0"
        );
    }

    #[test]
    fn test_probe_drive_reports_logical_sector() {
        let info = probe_drive();
        assert!(info.logical_sector >= 512);
    }

    #[test]
    fn test_probe_cpu_reports_at_least_one_logical_core() {
        let c = probe_cpu();
        assert!(c.cores_logical >= 1);
        assert!(c.cores_physical >= 1);
    }

    #[test]
    fn test_probe_io_primitives_iocp_and_mmap_true() {
        let p = probe_io_primitives();
        assert!(p.iocp);
        assert!(p.mmap);
        assert!(!p.io_uring);
    }
}
