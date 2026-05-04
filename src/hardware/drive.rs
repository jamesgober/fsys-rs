//! Storage-device probe — public types + delegation to per-platform
//! implementations.
//!
//! 0.5.0 replaces the 0.2.0 stubs with real probes implemented in
//! the crate-internal `probe` module (Linux: `/sys/block/`; Windows:
//! Win32 file APIs; macOS: `statvfs` + sysctl). PLP detection is
//! best-effort with `Unknown` returned when reliability cannot be
//! established — see [`super::PlpStatus`] for the rationale.

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
/// 0.5.0 populates these fields from real per-platform probes
/// (the crate-internal `probe` module) rather than the 0.2.0 stub
/// defaults. The probe
/// runs once per process (cached via [`super::info`]) and never fails
/// the handle — fields the probe couldn't determine fall back to the
/// values returned by [`DriveInfo::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveInfo {
    /// Coarse drive classification.
    pub kind: DriveKind,
    /// Power Loss Protection status as a tri-state. `Unknown` is the
    /// honest answer when the probe cannot reliably determine PLP
    /// (see [`super::PlpStatus`] for full rationale).
    pub plp: super::PlpStatus,
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
            plp: super::PlpStatus::Unknown,
            logical_sector: 512,
            physical_sector: 4_096,
            optimal_block: 65_536,
            queue_depth: 1,
            total_bytes: 0,
            available_bytes: 0,
        }
    }
}

/// Runs the per-platform drive probe.
///
/// Delegates to the crate-internal `probe::platform::probe_drive`.
/// The probe never panics; failures degrade to [`DriveInfo::default`].
#[must_use]
pub(super) fn probe() -> DriveInfo {
    super::probe::platform::probe_drive()
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
    fn test_default_plp_is_unknown() {
        assert_eq!(DriveInfo::default().plp, super::super::PlpStatus::Unknown);
    }

    #[test]
    fn test_kind_as_str_is_lowercase_kebab() {
        assert_eq!(DriveKind::Nvme.as_str(), "nvme");
        assert_eq!(DriveKind::SataSsd.as_str(), "sata-ssd");
        assert_eq!(DriveKind::Hdd.as_str(), "hdd");
        assert_eq!(DriveKind::Unknown.as_str(), "unknown");
    }

    #[test]
    fn test_probe_returns_well_formed_info() {
        // 0.5.0: probe is real per-platform. We cannot assert exact
        // values (they vary by hardware), but we can assert basic
        // well-formedness: sector sizes are at least 512, queue depth
        // is at least 1, and any reported total/available capacity is
        // non-zero on a real machine (degrades to default in
        // sandboxed CI without /sys/proc access — accept either).
        let info = probe();
        assert!(info.logical_sector >= 512);
        assert!(info.physical_sector >= 512);
        assert!(info.queue_depth >= 1);
    }
}
