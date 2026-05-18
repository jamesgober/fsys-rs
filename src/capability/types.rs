//! Public types for the capability cache (1.1.0).
//!
//! Everything in this file is part of the 1.x stable public API.
//! See `docs/STABILITY-1.0.md` for the SemVer contract that applies
//! to additions made in minor releases.

use std::fmt;

/// A snapshot of system capabilities relevant to backend selection.
///
/// Produced by [`crate::capability::capabilities()`] (cached) or
/// [`crate::capability::probe_capabilities_fresh()`] (uncached).
/// Field meanings are stable for the 1.x line; new fields may be
/// added in minor releases (the struct is `#[non_exhaustive]`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Capabilities {
    /// Cache schema version this snapshot was probed under. Must
    /// match [`crate::capability::CAPABILITY_CACHE_SCHEMA_VERSION`]
    /// or the on-disk cache is invalidated.
    pub schema_version: u32,
    /// fsys crate version that produced this snapshot. Mismatches
    /// against [`env!`]`("CARGO_PKG_VERSION")` trigger re-probing.
    pub fsys_version: String,
    /// Kernel version (Linux/macOS) or OS build (Windows) at probe
    /// time. Mismatches against the live value trigger re-probing.
    pub kernel_version: String,
    /// Target OS string (matches [`std::env::consts::OS`]).
    pub os_target: String,
    /// Wall-clock time when the probe ran, as seconds since the
    /// Unix epoch. Used by the 30-day age-based invalidation.
    pub probed_at_unix_secs: u64,

    /// `true` when `io_uring` is available on this kernel.
    pub io_uring: bool,
    /// io_uring feature flags detected at probe time (`COOP_TASKRUN`,
    /// `SINGLE_ISSUER`, `DEFER_TASKRUN`, etc.). Empty when
    /// `io_uring == false`.
    pub io_uring_features: Vec<IoUringFeature>,
    /// `true` when the active platform exposes NVMe passthrough
    /// commands (Linux 5.19+ with capable hardware; Windows with
    /// `IOCTL_STORAGE_PROTOCOL_COMMAND`).
    pub nvme_passthrough: bool,
    /// `true` when `O_DIRECT` / `F_NOCACHE` / `FILE_FLAG_NO_BUFFERING`
    /// is supported on the active platform's typical data filesystem.
    pub direct_io: bool,
    /// `true` when at least one drive on this host has confirmed
    /// Power-Loss Protection (vendor-lookup table hit, or NVMe VWC
    /// bit set to 0).
    pub plp_detected: bool,

    /// `true` when **every** SPDK precondition passed and at least
    /// one NVMe device is eligible for SPDK binding. When `false`,
    /// inspect [`Self::spdk_skip_reasons`] for the specific failures.
    pub spdk_eligible: bool,
    /// All preconditions that failed during the SPDK probe. Empty
    /// when [`Self::spdk_eligible`] is `true`. Multiple reasons may
    /// be present (e.g. "no hugepages AND no IOMMU"); fixing one
    /// without the others does not flip the eligibility bit.
    pub spdk_skip_reasons: Vec<SpdkSkipReason>,
    /// PCI addresses of NVMe devices that passed all probes and are
    /// usable by the SPDK backend (driver unbound or already bound
    /// to `vfio-pci` / `uio_pci_generic`). Empty when
    /// [`Self::spdk_eligible`] is `false`.
    pub spdk_eligible_devices: Vec<PciAddress>,

    /// Subset of [`crate::hardware::HardwareInfo`] needed for
    /// backend tuning decisions (sector sizes, queue depth, optimal
    /// block size). Stored in the cache so the cache is
    /// self-contained — consumers don't need to re-run the hardware
    /// probe to get tuning hints.
    pub hardware: HardwareSummary,
}

impl Capabilities {
    /// The cache schema version this build understands. A mismatch
    /// in the on-disk cache forces re-probing.
    pub const SCHEMA_VERSION: u32 = crate::capability::CAPABILITY_CACHE_SCHEMA_VERSION;

    /// Returns `true` when the SPDK backend is selectable on this
    /// host, assuming the `spdk` Cargo feature is also enabled at
    /// compile time.
    ///
    /// Equivalent to `self.spdk_eligible`. The accessor exists for
    /// readability at call sites in the `Builder::build` gating
    /// path and for forward-compatibility — future capability
    /// dimensions may collapse into this answer.
    #[must_use]
    #[inline]
    pub fn supports_spdk(&self) -> bool {
        self.spdk_eligible
    }

    /// Returns the first SPDK skip reason, if any.
    ///
    /// Useful when surfacing a single reason to a caller via
    /// [`crate::Error::SpdkUnavailable`]. The full list is on
    /// [`Self::spdk_skip_reasons`].
    #[must_use]
    #[inline]
    pub fn first_spdk_skip_reason(&self) -> Option<&SpdkSkipReason> {
        self.spdk_skip_reasons.first()
    }
}

/// Hardware tuning hints carried by the capability cache.
///
/// A read-only snapshot of the subset of [`crate::hardware::HardwareInfo`]
/// that's expensive enough to want cached on disk and stable enough
/// to be safely cached.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HardwareSummary {
    /// Drive kind label (`"nvme"` / `"sata-ssd"` / `"hdd"` /
    /// `"unknown"`) at probe time.
    pub drive_type: String,
    /// Preferred IO unit for the drive, in bytes.
    pub optimal_block_size: u32,
    /// NVMe queue depth (default 1 for non-NVMe drives).
    pub queue_depth: u32,
    /// Logical sector size, in bytes.
    pub sector_size_logical: u32,
    /// Physical sector size, in bytes. Advanced-format drives
    /// expose a 4 KiB physical sector behind a 512-byte logical
    /// emulation; large writes should be aligned to the physical
    /// size to avoid read-modify-write penalties.
    pub sector_size_physical: u32,

    /// Whether `io_uring` was available at probe time. Duplicated
    /// from [`Capabilities::io_uring`] for convenience — when the
    /// `HardwareSummary` is consumed separately from the full
    /// [`Capabilities`] (e.g. as a tuning hint by a backend
    /// constructor), this saves an indirection.
    pub io_uring: bool,
    /// Same shape as [`Capabilities::nvme_passthrough`].
    pub nvme_passthrough: bool,
    /// Same shape as [`Capabilities::direct_io`].
    pub direct_io: bool,
    /// Same shape as [`Capabilities::plp_detected`].
    pub plp_detected: bool,
}

impl HardwareSummary {
    /// Builds a `HardwareSummary` from the live hardware probe.
    ///
    /// Internal use only — callers that want a fresh summary should
    /// go through [`crate::capability::probe_capabilities_fresh()`]
    /// and read `cap.hardware`.
    pub(crate) fn from_live_probe() -> Self {
        let hw = crate::hardware::info();
        Self {
            drive_type: hw.drive.kind.as_str().to_string(),
            optimal_block_size: hw.drive.optimal_block,
            queue_depth: hw.drive.queue_depth,
            sector_size_logical: hw.drive.logical_sector,
            sector_size_physical: hw.drive.physical_sector,
            io_uring: hw.io_primitives.io_uring,
            nvme_passthrough: hw.io_primitives.nvme_passthrough,
            direct_io: hw.io_primitives.direct_io,
            plp_detected: matches!(hw.drive.plp, crate::hardware::PlpStatus::Yes),
        }
    }
}

/// A PCI bus/device/function address, as printed by `lspci` and the
/// Linux sysfs directory naming convention.
///
/// Format: `DDDD:BB:DD.F` — a 16-bit domain, 8-bit bus, 5-bit device,
/// 3-bit function. Domain 0000 is implicit on systems without multi-
/// domain support; we always emit and parse the full four-segment form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PciAddress {
    /// PCI segment / domain (typically `0`).
    pub domain: u16,
    /// PCI bus.
    pub bus: u8,
    /// PCI device.
    pub device: u8,
    /// PCI function.
    pub function: u8,
}

impl PciAddress {
    /// Constructs a new [`PciAddress`].
    #[must_use]
    #[inline]
    pub const fn new(domain: u16, bus: u8, device: u8, function: u8) -> Self {
        Self {
            domain,
            bus,
            device,
            function,
        }
    }

    /// Returns the canonical `DDDD:BB:DD.F` string representation.
    #[must_use]
    pub fn to_canonical(&self) -> String {
        format!(
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.domain, self.bus, self.device, self.function
        )
    }

    /// Parses a canonical `[DDDD:]BB:DD.F` address.
    ///
    /// Accepts both four-segment (`0000:00:1f.2`) and three-segment
    /// (`00:1f.2`) forms; the latter implies `domain = 0`. Returns
    /// `None` on any parse error — capability probing must never
    /// panic on malformed sysfs entries.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (domain, rest) = match s.split_once(':') {
            None => return None,
            Some((first, rest)) if rest.contains(':') => {
                let domain = u16::from_str_radix(first, 16).ok()?;
                (domain, rest)
            }
            Some(_) => (0u16, s),
        };
        let (bus_str, devfunc) = rest.split_once(':')?;
        let bus = u8::from_str_radix(bus_str, 16).ok()?;
        let (dev_str, func_str) = devfunc.split_once('.')?;
        let device = u8::from_str_radix(dev_str, 16).ok()?;
        let function = u8::from_str_radix(func_str, 16).ok()?;
        Some(Self {
            domain,
            bus,
            device,
            function,
        })
    }
}

impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_canonical())
    }
}

/// io_uring kernel feature flags surfaced by the capability probe.
///
/// The variants mirror the kernel's `IORING_SETUP_*` and
/// `IORING_FEAT_*` names, lowercased. Adding a new variant in a
/// minor release is non-breaking because the enum is
/// `#[non_exhaustive]` and the field is a `Vec` rather than a
/// fixed-shape bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IoUringFeature {
    /// `IORING_FEAT_FAST_POLL` — kernel-side fast-poll completion.
    FastPoll,
    /// `IORING_REGISTER_BUFFERS` — pre-registered IO buffer slots.
    RegisterBuffers,
    /// `IORING_REGISTER_FILES` — pre-registered fd slot table.
    RegisterFiles,
    /// `IORING_OP_URING_CMD` — pass-through device-command op.
    UringCmd,
    /// `IORING_SETUP_SUBMIT_ALL` — submission-batch-all behaviour.
    SubmitAll,
    /// `IORING_SETUP_COOP_TASKRUN` — cooperative task-run scheduling.
    CoopTaskrun,
    /// `IORING_SETUP_SINGLE_ISSUER` — single-issuer optimisation.
    SingleIssuer,
    /// `IORING_SETUP_DEFER_TASKRUN` — defer task-run to userspace.
    DeferTaskrun,
    /// `IORING_SETUP_SQPOLL` — kernel polling thread on the SQ.
    SqPoll,
}

impl IoUringFeature {
    /// Canonical lowercase name used in the cache file and rustdoc.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            IoUringFeature::FastPoll => "fast_poll",
            IoUringFeature::RegisterBuffers => "register_buffers",
            IoUringFeature::RegisterFiles => "register_files",
            IoUringFeature::UringCmd => "uring_cmd",
            IoUringFeature::SubmitAll => "submit_all",
            IoUringFeature::CoopTaskrun => "coop_taskrun",
            IoUringFeature::SingleIssuer => "single_issuer",
            IoUringFeature::DeferTaskrun => "defer_taskrun",
            IoUringFeature::SqPoll => "sqpoll",
        }
    }

    /// Parses a canonical name. Returns `None` for unknown strings
    /// (forward-compat: a cache file written by a newer fsys may
    /// list a feature this build does not know about; the unknown
    /// entry is dropped rather than erroring out the read).
    #[must_use]
    pub fn from_str_canonical(s: &str) -> Option<Self> {
        Some(match s {
            "fast_poll" => IoUringFeature::FastPoll,
            "register_buffers" => IoUringFeature::RegisterBuffers,
            "register_files" => IoUringFeature::RegisterFiles,
            "uring_cmd" => IoUringFeature::UringCmd,
            "submit_all" => IoUringFeature::SubmitAll,
            "coop_taskrun" => IoUringFeature::CoopTaskrun,
            "single_issuer" => IoUringFeature::SingleIssuer,
            "defer_taskrun" => IoUringFeature::DeferTaskrun,
            "sqpoll" => IoUringFeature::SqPoll,
            _ => return None,
        })
    }
}

impl fmt::Display for IoUringFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of the SPDK eligibility probe.
///
/// Returned by [`crate::capability::probe::spdk_eligibility()`].
/// `eligible == true` requires every precondition to pass AND at
/// least one usable NVMe device.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SpdkEligibility {
    /// `true` only when every precondition passed and at least one
    /// NVMe device is available.
    pub eligible: bool,
    /// All preconditions that failed. Empty when `eligible == true`.
    pub reasons_failed: Vec<SpdkSkipReason>,
    /// NVMe devices that passed individual probing (driver unbound
    /// or already bound to `vfio-pci` / `uio_pci_generic`).
    pub eligible_devices: Vec<PciAddress>,
}

/// Specific SPDK precondition failures.
///
/// Each variant explains exactly which check failed so an operator
/// can fix the underlying configuration. The enum is
/// `#[non_exhaustive]` so new check types can be added in minor
/// releases.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpdkSkipReason {
    /// SPDK is only supported on Linux. macOS and Windows always
    /// surface this reason; selecting [`crate::Method::Spdk`] on
    /// those platforms returns
    /// [`crate::Error::SpdkUnavailable`] with this reason.
    NotLinux,
    /// `/proc/meminfo` reports insufficient hugepages.
    ///
    /// SPDK requires hugepage-backed DMA buffers for the per-thread
    /// pre-allocated pool. The recommended floor is 1024 MiB
    /// reserved as hugepages (`HugePages_Total * Hugepagesize`).
    /// The minimum-viable floor is 256 MiB. Both are reported here
    /// so the operator can size the allocation appropriately.
    HugepagesNotConfigured {
        /// Total hugepage-backed memory currently reserved, in MiB.
        current_mb: u64,
        /// Recommended floor for production SPDK workloads, in MiB.
        recommended_mb: u64,
    },
    /// The calling process lacks `uid 0` and `CAP_SYS_ADMIN`.
    ///
    /// SPDK opens character devices in `/dev/vfio/` and pins
    /// hugepages, both of which require privilege. Run the
    /// application as root, set `CAP_SYS_ADMIN` via `setcap`, or
    /// configure a systemd unit with the appropriate
    /// `AmbientCapabilities=`.
    InsufficientPrivileges,
    /// No NVMe devices were found on the PCI bus.
    NoNvmeDevices,
    /// NVMe devices were found, but **every** one of them is bound
    /// exclusively to the kernel `nvme` driver and cannot be used
    /// by SPDK without rebinding.
    ///
    /// The remediation is to detach the listed device from `nvme`
    /// and bind it to `vfio-pci` or `uio_pci_generic`. See
    /// `docs/SPDK.md` for the exact `setpci` / `driverctl` commands.
    AllDevicesInUse {
        /// PCI addresses of the kernel-bound devices.
        devices: Vec<PciAddress>,
    },
    /// `/sys/kernel/iommu_groups/` is missing or empty.
    ///
    /// IOMMU is required for the `vfio-pci` path (the safer of the
    /// two SPDK driver options). Without IOMMU, only the
    /// `uio_pci_generic` path is available — it works, but it gives
    /// the SPDK process direct DMA to physical memory with no
    /// hardware mediation. Enable `intel_iommu=on` (Intel) or
    /// `amd_iommu=on` (AMD) on the kernel command line and reboot.
    IommuNotConfigured,
    /// Fewer than 4 CPU cores are available on this host.
    ///
    /// SPDK's polling-thread architecture realistically needs four
    /// or more cores: one for the runtime + workload, two-to-three
    /// for polling threads. On smaller hosts, the kernel + io_uring
    /// path is faster overall because the SPDK polling overhead
    /// dominates the syscall savings.
    InsufficientCores {
        /// Cores available to the process at probe time.
        available: usize,
        /// Recommended floor.
        recommended: usize,
    },
    /// The `fsys-spdk` companion crate could not be loaded.
    ///
    /// Surfaced when the `spdk` Cargo feature is enabled but the
    /// runtime dynamic library (libspdk / librte_*) is missing
    /// from the search path. The build linked successfully but
    /// runtime initialisation will fail; we surface this here
    /// rather than panicking on first SPDK call.
    SpdkLibraryNotFound,
}

impl fmt::Display for SpdkSkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpdkSkipReason::NotLinux => {
                f.write_str("SPDK requires Linux; the current platform is unsupported")
            }
            SpdkSkipReason::HugepagesNotConfigured {
                current_mb,
                recommended_mb,
            } => write!(
                f,
                "hugepages not configured ({current_mb} MB allocated, {recommended_mb} MB recommended)"
            ),
            SpdkSkipReason::InsufficientPrivileges => {
                f.write_str("insufficient privileges (uid 0 or CAP_SYS_ADMIN required)")
            }
            SpdkSkipReason::NoNvmeDevices => f.write_str("no NVMe devices detected on the PCI bus"),
            SpdkSkipReason::AllDevicesInUse { devices } => write!(
                f,
                "all {} NVMe device(s) are currently bound to the kernel 'nvme' driver",
                devices.len()
            ),
            SpdkSkipReason::IommuNotConfigured => {
                f.write_str("IOMMU not configured (/sys/kernel/iommu_groups missing or empty)")
            }
            SpdkSkipReason::InsufficientCores {
                available,
                recommended,
            } => write!(
                f,
                "insufficient cores ({available} available, {recommended} recommended)"
            ),
            SpdkSkipReason::SpdkLibraryNotFound => {
                f.write_str("SPDK runtime library not found on the dynamic loader search path")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pci_address_canonical_format() {
        let pci = PciAddress::new(0, 0x1f, 0x03, 2);
        assert_eq!(pci.to_canonical(), "0000:1f:03.2");
        assert_eq!(pci.to_string(), "0000:1f:03.2");
    }

    #[test]
    fn test_pci_address_parse_four_segment() {
        let pci = PciAddress::parse("0000:1f:03.2").expect("parse");
        assert_eq!(pci, PciAddress::new(0, 0x1f, 0x03, 2));
    }

    #[test]
    fn test_pci_address_parse_three_segment_implies_domain_zero() {
        let pci = PciAddress::parse("1f:03.2").expect("parse");
        assert_eq!(pci, PciAddress::new(0, 0x1f, 0x03, 2));
    }

    #[test]
    fn test_pci_address_parse_rejects_garbage() {
        assert!(PciAddress::parse("").is_none());
        assert!(PciAddress::parse("nonsense").is_none());
        assert!(PciAddress::parse("xx:yy:zz.w").is_none());
        assert!(PciAddress::parse("0000:1f:03").is_none()); // missing .F
    }

    #[test]
    fn test_pci_address_round_trip_full_form() {
        let original = PciAddress::new(0xABCD, 0xEF, 0x12, 5);
        let parsed = PciAddress::parse(&original.to_canonical()).expect("parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn test_io_uring_feature_as_str_round_trip() {
        for f in [
            IoUringFeature::FastPoll,
            IoUringFeature::RegisterBuffers,
            IoUringFeature::RegisterFiles,
            IoUringFeature::UringCmd,
            IoUringFeature::SubmitAll,
            IoUringFeature::CoopTaskrun,
            IoUringFeature::SingleIssuer,
            IoUringFeature::DeferTaskrun,
            IoUringFeature::SqPoll,
        ] {
            let s = f.as_str();
            assert_eq!(IoUringFeature::from_str_canonical(s), Some(f));
        }
    }

    #[test]
    fn test_io_uring_feature_from_str_rejects_unknown() {
        assert!(IoUringFeature::from_str_canonical("not_a_feature").is_none());
    }

    #[test]
    fn test_spdk_skip_reason_display_is_human_readable() {
        assert!(SpdkSkipReason::NotLinux
            .to_string()
            .to_ascii_lowercase()
            .contains("linux"));
        assert!(SpdkSkipReason::InsufficientPrivileges
            .to_string()
            .to_ascii_lowercase()
            .contains("privileges"));
        assert!(SpdkSkipReason::NoNvmeDevices
            .to_string()
            .to_ascii_lowercase()
            .contains("nvme"));
        assert!(SpdkSkipReason::IommuNotConfigured
            .to_string()
            .to_ascii_lowercase()
            .contains("iommu"));
        assert!(SpdkSkipReason::SpdkLibraryNotFound
            .to_string()
            .to_ascii_lowercase()
            .contains("spdk"));
    }

    #[test]
    fn test_spdk_skip_reason_hugepages_includes_numbers() {
        let r = SpdkSkipReason::HugepagesNotConfigured {
            current_mb: 0,
            recommended_mb: 1024,
        };
        let s = r.to_string();
        assert!(s.contains('0'));
        assert!(s.contains("1024"));
    }

    #[test]
    fn test_spdk_skip_reason_all_devices_in_use_includes_count() {
        let r = SpdkSkipReason::AllDevicesInUse {
            devices: vec![PciAddress::new(0, 0, 0, 0)],
        };
        let s = r.to_string();
        assert!(s.contains('1'));
    }

    #[test]
    fn test_spdk_skip_reason_insufficient_cores_includes_numbers() {
        let r = SpdkSkipReason::InsufficientCores {
            available: 2,
            recommended: 4,
        };
        let s = r.to_string();
        assert!(s.contains('2'));
        assert!(s.contains('4'));
    }
}
