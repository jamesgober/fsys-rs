//! Storage-device probe.
//!
//! Real device interrogation (NVMe Identify, sector-size lookup, PLP
//! detection, capacity probing) requires platform-specific syscalls
//! and IOCTLs that fsys does not yet pull in. The `0.0.2` foundation
//! returns a [`DriveInfo`] populated with conservative, universally
//! safe defaults so the rest of the crate can rely on the struct
//! always being shaped correctly.
//!
//! Real probing lands in `0.0.5`; each deferred field is marked with a
//! `TODO(0.0.5)` comment.

/// Coarse classification of the storage device.
///
/// Used by [`crate::hardware::DriveInfo::kind`] to drive method
/// selection ladders. `Unknown` is the only value the foundation
/// layer ever produces; real probing in `0.0.5` widens this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum DriveKind {
    /// NVMe SSD (PCIe or fabric).
    Nvme,
    /// SATA / SAS SSD.
    SataSsd,
    /// Spinning disk.
    Hdd,
    /// Probe could not classify the device or has not run yet.
    #[default]
    Unknown,
}

impl DriveKind {
    /// Returns a stable lowercase name for this kind.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            DriveKind::Nvme => "nvme",
            DriveKind::SataSsd => "sata-ssd",
            DriveKind::Hdd => "hdd",
            DriveKind::Unknown => "unknown",
        }
    }
}

/// Snapshot of the storage device fsys currently sees.
///
/// All fields are populated with conservative defaults in `0.0.2`.
/// Real values arrive in `0.0.5` once the per-platform device probes
/// land. The struct is shaped final — only the values change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveInfo {
    /// Coarse drive classification.
    pub kind: DriveKind,
    /// Whether the device has Power Loss Protection (a battery / cap
    /// that lets the controller flush its cache on power loss).
    /// Defaults to `false` until probed.
    pub plp: bool,
    /// Logical sector size in bytes. `512` is the safest default and
    /// is reported by every commodity device at the OS level even
    /// when the underlying physical sector is `4096`.
    pub logical_sector: u32,
    /// Physical sector size in bytes. `4096` is the modern default.
    pub physical_sector: u32,
    /// Optimal IO transfer size hinted by the device, in bytes. `64
    /// KiB` is a conservative default that performs well across NVMe,
    /// SATA SSD, and HDD.
    pub optimal_block: u32,
    /// NVMe submission-queue depth. `1` for non-NVMe devices and as
    /// the default until probing succeeds.
    pub queue_depth: u32,
    /// Total capacity of the device in bytes. `0` means unknown.
    pub total_bytes: u64,
    /// Available (free) capacity in bytes. `0` means unknown.
    pub available_bytes: u64,
}

impl Default for DriveInfo {
    fn default() -> Self {
        Self {
            kind: DriveKind::Unknown,
            plp: false,
            logical_sector: 512,
            physical_sector: 4_096,
            optimal_block: 65_536,
            queue_depth: 1,
            total_bytes: 0,
            available_bytes: 0,
        }
    }
}

/// Runs the foundation-layer drive probe.
///
/// Always returns the [`DriveInfo`] default in `0.0.2`. Real
/// platform-specific probing (NVMe Identify on Linux, IOCTL on
/// Windows, IOKit on macOS) is implemented in `0.0.5`.
#[must_use]
pub(super) fn probe() -> DriveInfo {
    // TODO(0.0.5): replace with real per-platform probes.
    //  - Linux: open the block device under /sys/block, parse queue
    //    parameters, issue NVMe Identify via ioctl when applicable.
    //  - Windows: DeviceIoControl with IOCTL_STORAGE_QUERY_PROPERTY.
    //  - macOS: IOKit IORegistry queries.
    DriveInfo::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_kind_is_unknown() {
        assert_eq!(DriveInfo::default().kind, DriveKind::Unknown);
    }

    #[test]
    fn test_default_logical_sector_is_512_bytes() {
        assert_eq!(DriveInfo::default().logical_sector, 512);
    }

    #[test]
    fn test_default_physical_sector_is_4_kib() {
        assert_eq!(DriveInfo::default().physical_sector, 4_096);
    }

    #[test]
    fn test_default_optimal_block_is_64_kib() {
        assert_eq!(DriveInfo::default().optimal_block, 65_536);
    }

    #[test]
    fn test_default_queue_depth_is_one() {
        assert_eq!(DriveInfo::default().queue_depth, 1);
    }

    #[test]
    fn test_default_plp_is_false() {
        assert!(!DriveInfo::default().plp);
    }

    #[test]
    fn test_kind_as_str_is_lowercase_kebab() {
        assert_eq!(DriveKind::Nvme.as_str(), "nvme");
        assert_eq!(DriveKind::SataSsd.as_str(), "sata-ssd");
        assert_eq!(DriveKind::Hdd.as_str(), "hdd");
        assert_eq!(DriveKind::Unknown.as_str(), "unknown");
    }

    #[test]
    fn test_probe_matches_default_in_foundation_phase() {
        assert_eq!(probe(), DriveInfo::default());
    }
}
