//! Hardware probing.
//!
//! [`HardwareInfo`] collects everything the rest of the crate needs to
//! know about the machine it is running on: the storage device, the
//! amount of RAM, the CPU, and which kernel-level IO primitives are
//! reachable. The struct is probed once on first access and cached for
//! the lifetime of the process; only [`memory()`] re-reads on every call
//! because RAM availability is a moving target.
//!
//! ## Status
//!
//! All probes in `0.0.2` return correctly-shaped data, but most fields
//! carry conservative defaults. Real device interrogation —
//! NVMe Identify, sector-size lookup, PLP detection, capacity probing,
//! `sysinfo`/`GetSystemInfo` for memory — lands in `0.0.5`. Each
//! deferred field is marked with a `TODO(0.0.5)` comment in the
//! corresponding sub-module.

use std::sync::OnceLock;

pub mod cpu;
pub mod drive;
pub mod io_primitives;
pub mod memory;

pub use crate::hardware::cpu::{CpuFeatures, CpuInfo};
pub use crate::hardware::drive::{DriveInfo, DriveKind};
pub use crate::hardware::io_primitives::IoPrimitives;
pub use crate::hardware::memory::MemoryInfo;

/// Aggregated hardware snapshot.
///
/// Owns the immutable parts of the probe (drive identification, CPU,
/// IO primitives). Memory is re-probed live via [`memory()`] because the
/// available figure changes constantly.
#[derive(Debug, Clone)]
pub struct HardwareInfo {
    /// Storage device the probe believes the application is running on.
    pub drive: DriveInfo,
    /// Memory snapshot taken at the time the cache was populated. Use
    /// [`memory()`] for a fresh value.
    pub memory: MemoryInfo,
    /// CPU information.
    pub cpu: CpuInfo,
    /// Kernel-level IO primitives the active platform exposes.
    pub io_primitives: IoPrimitives,
}

static HARDWARE_INFO: OnceLock<HardwareInfo> = OnceLock::new();
static DRIVE_INFO: OnceLock<DriveInfo> = OnceLock::new();
static CPU_INFO: OnceLock<CpuInfo> = OnceLock::new();
static IO_PRIMITIVES: OnceLock<IoPrimitives> = OnceLock::new();

/// Returns the cached [`HardwareInfo`], probing on first call.
///
/// The probe never panics. Fields that cannot be determined fall back
/// to documented defaults and the rest of the crate continues to
/// operate.
///
/// # Examples
///
/// ```
/// let hw = fsys::hardware::info();
/// assert!(hw.cpu.cores_logical >= 1);
/// ```
#[must_use]
pub fn info() -> &'static HardwareInfo {
    HARDWARE_INFO.get_or_init(|| HardwareInfo {
        drive: drive::probe(),
        memory: memory::probe(),
        cpu: cpu::probe(),
        io_primitives: io_primitives::probe(),
    })
}

/// Returns the cached [`DriveInfo`], probing on first call.
#[must_use]
pub fn drive() -> &'static DriveInfo {
    DRIVE_INFO.get_or_init(drive::probe)
}

/// Returns a fresh [`MemoryInfo`] snapshot.
///
/// Unlike the other accessors, this does **not** cache. Memory
/// availability changes constantly; callers expect a live reading.
#[must_use]
pub fn memory() -> MemoryInfo {
    memory::probe()
}

/// Returns the cached [`CpuInfo`], probing on first call.
#[must_use]
pub fn cpu() -> &'static CpuInfo {
    CPU_INFO.get_or_init(cpu::probe)
}

/// Returns the cached [`IoPrimitives`] availability map, probing on
/// first call.
#[must_use]
pub fn io_primitives() -> &'static IoPrimitives {
    IO_PRIMITIVES.get_or_init(io_primitives::probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_info_returns_consistent_reference() {
        let a = info() as *const HardwareInfo;
        let b = info() as *const HardwareInfo;
        assert_eq!(a, b);
    }

    #[test]
    fn test_info_cpu_has_at_least_one_core() {
        assert!(info().cpu.cores_logical >= 1);
    }

    #[test]
    fn test_drive_returns_consistent_reference() {
        let a = drive() as *const DriveInfo;
        let b = drive() as *const DriveInfo;
        assert_eq!(a, b);
    }

    #[test]
    fn test_memory_does_not_cache_a_static_reference() {
        // Both calls return owned MemoryInfo values; the function does
        // not return a `&'static`.
        let _: MemoryInfo = memory();
        let _: MemoryInfo = memory();
    }

    #[test]
    fn test_io_primitives_consistent_reference() {
        let a = io_primitives() as *const IoPrimitives;
        let b = io_primitives() as *const IoPrimitives;
        assert_eq!(a, b);
    }
}
