//! macOS-specific hardware probe.
//!
//! Probes use:
//! - Memory: `sysctlbyname("hw.memsize")` for total;
//!   `host_statistics64(HOST_VM_INFO64)` is the gold standard for
//!   available, but `sysctlbyname("vm.page_free_count")` ×
//!   `hw.pagesize` is a serviceable approximation that requires no
//!   Mach-port bindings. We use the simpler approximation in 0.5.0.
//! - Drive capacity / free space: `statvfs(2)`.
//! - Drive sector sizes: `fcntl(F_GETPATH)` + `statfs64`'s `f_bsize`.
//! - CPU: `sysctlbyname("hw.physicalcpu", "hw.logicalcpu",
//!   "hw.l1icachesize", "hw.l2cachesize", "hw.l3cachesize")`.
//! - PLP: deferred to 0.6.0 alongside IOKit refinement.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::PlpStatus;
use crate::hardware::cpu::CpuInfo;
use crate::hardware::drive::{DriveInfo, DriveKind};
use crate::hardware::io_primitives::IoPrimitives;
use crate::hardware::memory::MemoryInfo;

// ─────────────────────────────────────────────────────────────────────────────
// Drive
// ─────────────────────────────────────────────────────────────────────────────

/// Probes the storage device hosting the current working directory.
pub(crate) fn probe_drive() -> DriveInfo {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return DriveInfo::default(),
    };

    let mut info = DriveInfo::default();

    if let Some((total, available, fr_size)) = statvfs_capacity(&cwd) {
        info.total_bytes = total;
        info.available_bytes = available;
        if fr_size > 0 {
            // statvfs reports the filesystem's allocation block size as
            // `f_frsize`; that's a reasonable optimal-block hint on
            // macOS where lower-level sector info is harder to reach.
            info.optimal_block = fr_size as u32;
            // Use the same as physical sector when we can't probe
            // separately — accurate enough for alignment purposes on
            // APFS / HFS+.
            info.physical_sector = fr_size.min(4_096) as u32;
        }
    }

    // Drive kind: macOS hides the underlying transport behind APFS's
    // logical-volume model. Without IOKit (0.6.0), we conservatively
    // default to Unknown — the Auto ladder treats Unknown as
    // SSD-class anyway on macOS.
    info.kind = DriveKind::Unknown;

    let _: PlpStatus = PlpStatus::Unknown;
    info
}

fn statvfs_capacity(path: &Path) -> Option<(u64, u64, u64)> {
    let cstr = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is plain old data; zero-init is a valid
    // initial bit pattern; the syscall fully populates the struct
    // before any field is read.
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: cstr is a valid NUL-terminated path string; `&mut sv`
    // is a valid `*mut statvfs`.
    let ret = unsafe { libc::statvfs(cstr.as_ptr(), &mut sv) };
    if ret != 0 {
        return None;
    }
    let frsize = sv.f_frsize as u64;
    let total = (sv.f_blocks as u64).saturating_mul(frsize);
    let available = (sv.f_bavail as u64).saturating_mul(frsize);
    Some((total, available, frsize))
}

// ─────────────────────────────────────────────────────────────────────────────
// Memory
// ─────────────────────────────────────────────────────────────────────────────

/// Probes total and available memory.
///
/// Total via `sysctlbyname("hw.memsize")`. Available is approximated
/// from `vm.page_free_count` × `hw.pagesize`. The Mach-port-based
/// `host_statistics64(HOST_VM_INFO64)` would give a more nuanced
/// answer (free + active + inactive + speculative); 0.5.0 uses the
/// simpler sysctl path.
pub(crate) fn probe_memory() -> MemoryInfo {
    let total = sysctl_u64("hw.memsize").unwrap_or(0);
    let page_size = sysctl_u64("hw.pagesize").unwrap_or(4096);
    let pages_free = sysctl_u64("vm.page_free_count").unwrap_or(0);
    MemoryInfo {
        total_bytes: total,
        available_bytes: pages_free.saturating_mul(page_size),
    }
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let cname = CString::new(name).ok()?;
    let mut value: u64 = 0;
    let mut size: libc::size_t = std::mem::size_of::<u64>();
    // SAFETY: `cname` is NUL-terminated; `&mut value` and `&mut size`
    // are valid out-pointers; passing `null_mut()`/0 for newp/newlen
    // signals a read-only sysctl. The kernel writes at most
    // `size_of::<u64>()` bytes to `value`.
    let ret = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut value as *mut u64 as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 || size == 0 {
        return None;
    }
    Some(value)
}

fn sysctl_i32(name: &str) -> Option<i32> {
    let cname = CString::new(name).ok()?;
    let mut value: i32 = 0;
    let mut size: libc::size_t = std::mem::size_of::<i32>();
    // SAFETY: same contract as `sysctl_u64`.
    let ret = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut value as *mut i32 as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 || size == 0 {
        return None;
    }
    Some(value)
}

// ─────────────────────────────────────────────────────────────────────────────
// CPU
// ─────────────────────────────────────────────────────────────────────────────

/// Probes CPU info via `sysctlbyname`.
pub(crate) fn probe_cpu() -> CpuInfo {
    let cores_logical = sysctl_i32("hw.logicalcpu")
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .and_then(|n| u32::try_from(n.get()).ok())
        })
        .unwrap_or(1);

    let cores_physical = sysctl_i32("hw.physicalcpu")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(cores_logical);

    let cache_l1 = sysctl_u64("hw.l1icachesize").unwrap_or(0) as usize;
    let cache_l2 = sysctl_u64("hw.l2cachesize").unwrap_or(0) as usize;
    let cache_l3 = sysctl_u64("hw.l3cachesize").unwrap_or(0) as usize;

    CpuInfo {
        cores_logical,
        cores_physical,
        features: super::super::cpu::runtime_features(),
        cache_l1,
        cache_l2,
        cache_l3,
    }
}

// 0.9.2: `detect_compile_time_features` removed; CPU feature
// detection is now runtime-dispatched via `cpu::runtime_features()`.
// See `src/hardware/cpu.rs` for rationale.

// ─────────────────────────────────────────────────────────────────────────────
// IO primitives
// ─────────────────────────────────────────────────────────────────────────────

/// Probes which kernel-level IO primitives are reachable on macOS.
///
/// `kqueue` is universal on macOS / BSDs. `mmap` is universal.
/// `F_NOCACHE` for direct-IO is universal at the API level (some
/// filesystems still reject it). No `io_uring`. No NVMe passthrough
/// in 0.5.0.
pub(crate) fn probe_io_primitives() -> IoPrimitives {
    IoPrimitives {
        io_uring: false,
        iocp: false,
        kqueue: true,
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
        assert!(m.total_bytes > 0);
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
    }

    #[test]
    fn test_probe_io_primitives_kqueue_and_mmap_true() {
        let p = probe_io_primitives();
        assert!(p.kqueue);
        assert!(p.mmap);
    }
}
