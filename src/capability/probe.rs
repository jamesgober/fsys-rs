//! Light + deep capability probes (1.1.0).
//!
//! Split per-the brief:
//!
//! - **Light probe** — sub-millisecond. Reads `/proc/meminfo`, OS
//!   capabilities bitmask, [`std::thread::available_parallelism`]. Runs
//!   on every fresh probe.
//! - **Deep probe** — tens of milliseconds. Walks
//!   `/sys/bus/pci/devices/`, parses each device's `class`, checks
//!   `driver` symlinks, enumerates `/sys/kernel/iommu_groups/`. Runs
//!   alongside the light probe today; future work splits it so the
//!   deep probe is only run when the answer might matter (e.g. on
//!   first SPDK-related accessor read).
//!
//! Every probe **never panics** and **always returns a result**.
//! Sub-probes that cannot reach their data source (missing
//! `/proc/meminfo`, denied PCI enumeration, etc.) record the failure
//! as a [`SpdkSkipReason`] on the returned [`SpdkEligibility`]
//! instead of erroring out the whole probe.

use super::types::{IoUringFeature, PciAddress, SpdkEligibility, SpdkSkipReason};

/// Hugepage floor (in MiB) the SPDK backend is *eligible* with. Below
/// this, the probe records [`SpdkSkipReason::HugepagesNotConfigured`].
///
/// The brief specifies "Eligible if `HugePages_Total * Hugepagesize >=
/// 256 MB`." 256 MiB is enough for SPDK to initialise; running with
/// only that floor will OOM the DMA buffer pool under sustained load.
pub(crate) const SPDK_HUGEPAGE_MIN_MB: u64 = 256;

/// Recommended hugepage allocation (in MiB) for production SPDK use.
/// Surfaced in the [`SpdkSkipReason::HugepagesNotConfigured`] error
/// message so operators see the target value.
pub(crate) const SPDK_HUGEPAGE_RECOMMENDED_MB: u64 = 1024;

/// Minimum CPU core count for SPDK eligibility. Below this, the
/// polling-thread architecture spends more cycles on its own
/// scheduling than it saves on kernel syscalls.
pub(crate) const SPDK_MIN_CORES: usize = 4;

/// Runs the full SPDK eligibility probe.
///
/// Returns a [`SpdkEligibility`] aggregating every precondition's
/// pass/fail status plus the list of usable NVMe devices. `eligible`
/// is `true` if and only if every individual precondition passed AND
/// at least one device is bindable.
#[must_use]
pub fn spdk_eligibility() -> SpdkEligibility {
    spdk_eligibility_with_root(SysfsRoot::live())
}

/// Internal entry point parameterised on the sysfs root so unit tests
/// can supply synthetic filesystem trees.
pub(crate) fn spdk_eligibility_with_root(root: SysfsRoot<'_>) -> SpdkEligibility {
    if !cfg!(target_os = "linux") {
        return SpdkEligibility {
            eligible: false,
            reasons_failed: vec![SpdkSkipReason::NotLinux],
            eligible_devices: Vec::new(),
        };
    }

    let mut reasons: Vec<SpdkSkipReason> = Vec::new();

    // Check 1: hugepages.
    let hugepages_mb = root.hugepages_total_mb();
    if hugepages_mb < SPDK_HUGEPAGE_MIN_MB {
        reasons.push(SpdkSkipReason::HugepagesNotConfigured {
            current_mb: hugepages_mb,
            recommended_mb: SPDK_HUGEPAGE_RECOMMENDED_MB,
        });
    }

    // Check 2: privileges.
    if !root.has_sys_admin_capability() {
        reasons.push(SpdkSkipReason::InsufficientPrivileges);
    }

    // Check 3 + 4: NVMe devices + kernel-bound rejection.
    let devices = root.enumerate_nvme_devices();
    if devices.is_empty() {
        reasons.push(SpdkSkipReason::NoNvmeDevices);
    }
    let mut eligible_devices: Vec<PciAddress> = Vec::new();
    let mut kernel_bound: Vec<PciAddress> = Vec::new();
    for dev in &devices {
        match root.nvme_driver_binding(dev) {
            DriverBinding::Unbound | DriverBinding::Vfio | DriverBinding::Uio => {
                eligible_devices.push(dev.clone());
            }
            DriverBinding::KernelNvme => kernel_bound.push(dev.clone()),
        }
    }
    // Only flag "all in use" when devices existed but none are usable
    // — covered by NoNvmeDevices when the device list is empty.
    if !devices.is_empty() && eligible_devices.is_empty() {
        reasons.push(SpdkSkipReason::AllDevicesInUse {
            devices: kernel_bound,
        });
    }

    // Check 5: IOMMU groups configured.
    if !root.iommu_groups_present() {
        reasons.push(SpdkSkipReason::IommuNotConfigured);
    }

    // Check 6: CPU cores.
    let cores = root.available_cores();
    if cores < SPDK_MIN_CORES {
        reasons.push(SpdkSkipReason::InsufficientCores {
            available: cores,
            recommended: SPDK_MIN_CORES,
        });
    }

    SpdkEligibility {
        eligible: reasons.is_empty() && !eligible_devices.is_empty(),
        reasons_failed: reasons,
        eligible_devices,
    }
}

/// Probe io_uring kernel feature flags.
///
/// 1.1.0 surfaces a conservative feature set built from
/// [`crate::hardware::io_primitives()`] and the existing
/// crate-internal `platform::iouring_features` runtime probe. A full
/// `IORING_REGISTER_PROBE`-based enumeration is captured by the
/// existing infrastructure; this function just re-exports it in the
/// new `IoUringFeature` shape for the cache.
#[must_use]
pub fn io_uring_features() -> Vec<IoUringFeature> {
    #[cfg(target_os = "linux")]
    {
        // Translate the existing platform-level features struct into
        // the cache-stable enum. 1.1.0 surfaces the three elite-tier
        // setup flags that 0.9.4's existing probe detects:
        // `COOP_TASKRUN`, `SINGLE_ISSUER`, `DEFER_TASKRUN`. The other
        // `IoUringFeature` variants (`FastPoll`, `RegisterBuffers`,
        // `RegisterFiles`, `UringCmd`, `SubmitAll`, `SqPoll`) are
        // present in the public enum for forward compatibility — a
        // future minor release that adds an `IORING_REGISTER_PROBE`-
        // based enumeration will fill them in without breaking
        // 1.1.0 readers.
        let raw = crate::platform::iouring_features::features();
        let mut v = Vec::new();
        if raw.coop_taskrun {
            v.push(IoUringFeature::CoopTaskrun);
        }
        if raw.single_issuer {
            v.push(IoUringFeature::SingleIssuer);
        }
        if raw.defer_taskrun {
            v.push(IoUringFeature::DeferTaskrun);
        }
        v
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Returns the running kernel version (Linux/macOS) or build label
/// (Windows / other) for cache invalidation.
///
/// **Linux:** reads `/proc/sys/kernel/osrelease` — the standard
/// kernel-version string. Falls back to `"unknown"` if the file
/// is unreadable.
///
/// **macOS:** the `sysctl` `kern.osrelease` would be the analogue;
/// 1.1.0 returns `"macos"` as a coarse cache key (the SPDK probe
/// always reports `NotLinux` on macOS, so the kernel version doesn't
/// actually matter for invalidation here).
///
/// **Windows:** returns `"windows"` for the same reason.
#[must_use]
pub fn kernel_version_string() -> String {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            Ok(s) => s.trim().to_string(),
            Err(_) => "unknown".to_string(),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::consts::OS.to_string()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Sysfs / proc abstraction for testability
// ──────────────────────────────────────────────────────────────────────

/// Driver-binding state of a single NVMe device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriverBinding {
    /// No driver bound (`/sys/bus/pci/devices/<addr>/driver` missing).
    Unbound,
    /// Bound to the kernel `nvme` driver.
    KernelNvme,
    /// Bound to `vfio-pci` (SPDK-ready, IOMMU-mediated).
    Vfio,
    /// Bound to `uio_pci_generic` (SPDK-ready, no IOMMU).
    Uio,
}

/// Abstraction over the sysfs / procfs filesystem so unit tests can
/// supply synthetic trees without root access.
///
/// The `'a` lifetime is the borrowed root path string; for the live
/// system it's `'static`, for tests it's the test scope.
pub(crate) struct SysfsRoot<'a> {
    /// Path prefix that stands in for `/`. Live system: `""`.
    pub(crate) root: &'a str,
    /// Override hugepage MiB (for tests). `None` reads /proc.
    pub(crate) hugepages_override_mb: Option<u64>,
    /// Override "has CAP_SYS_ADMIN" (for tests). `None` reads
    /// /proc/self/status.
    pub(crate) sys_admin_override: Option<bool>,
    /// Override core count (for tests). `None` uses
    /// `std::thread::available_parallelism`.
    pub(crate) cores_override: Option<usize>,
}

impl<'a> SysfsRoot<'a> {
    /// Live-system root: reads real `/sys` and `/proc`.
    pub(crate) fn live() -> SysfsRoot<'static> {
        SysfsRoot {
            root: "",
            hugepages_override_mb: None,
            sys_admin_override: None,
            cores_override: None,
        }
    }

    fn hugepages_total_mb(&self) -> u64 {
        if let Some(v) = self.hugepages_override_mb {
            return v;
        }
        // /proc/meminfo lines look like:
        //   HugePages_Total:      1024
        //   Hugepagesize:       2048 kB
        let path = format!("{}/proc/meminfo", self.root);
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let mut total: Option<u64> = None;
        let mut size_kb: Option<u64> = None;
        for line in contents.lines() {
            if let Some(v) = parse_meminfo_field(line, "HugePages_Total:") {
                total = Some(v);
            } else if let Some(v) = parse_meminfo_field(line, "Hugepagesize:") {
                size_kb = Some(v);
            }
        }
        match (total, size_kb) {
            (Some(t), Some(kb)) => t.saturating_mul(kb) / 1024,
            _ => 0,
        }
    }

    fn has_sys_admin_capability(&self) -> bool {
        if let Some(v) = self.sys_admin_override {
            return v;
        }
        #[cfg(target_os = "linux")]
        {
            // uid 0 is sufficient.
            // SAFETY: `getuid` is signal-safe and returns an unsigned
            // value; calling it never has side effects we care about.
            let uid = unsafe { libc::getuid() };
            if uid == 0 {
                return true;
            }
            // Otherwise, parse CapEff from /proc/self/status. The
            // CAP_SYS_ADMIN bit is bit 21 (0x200000) in the lower
            // 32 bits of the effective capability mask.
            let path = format!("{}/proc/self/status", self.root);
            let contents = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => return false,
            };
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("CapEff:") {
                    if let Ok(mask) = u64::from_str_radix(rest.trim(), 16) {
                        const CAP_SYS_ADMIN: u64 = 1 << 21;
                        return mask & CAP_SYS_ADMIN != 0;
                    }
                }
            }
            false
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn enumerate_nvme_devices(&self) -> Vec<PciAddress> {
        let path = format!("{}/sys/bus/pci/devices", self.root);
        let entries = match std::fs::read_dir(&path) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(pci) = PciAddress::parse(&name) else {
                continue;
            };
            // PCI class for NVMe controllers (0x010802).
            // The class file may be `0x010802` or `0x010802ff` depending
            // on the kernel; match by prefix to be safe.
            let class_path = format!("{}/sys/bus/pci/devices/{}/class", self.root, name);
            let class = match std::fs::read_to_string(&class_path) {
                Ok(c) => c.trim().to_string(),
                Err(_) => continue,
            };
            if class.starts_with("0x010802") {
                out.push(pci);
            }
        }
        // Stable sort for predictable test output.
        out.sort_by_key(|a| a.to_canonical());
        out
    }

    fn nvme_driver_binding(&self, pci: &PciAddress) -> DriverBinding {
        let driver_path = format!(
            "{}/sys/bus/pci/devices/{}/driver",
            self.root,
            pci.to_canonical()
        );
        let target = match std::fs::read_link(&driver_path) {
            Ok(t) => t,
            Err(_) => return DriverBinding::Unbound,
        };
        // The symlink target ends with the driver name — e.g.
        //   ../../../../bus/pci/drivers/nvme
        // We just look at the last path component.
        let name = target
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        match name.as_str() {
            "nvme" => DriverBinding::KernelNvme,
            "vfio-pci" => DriverBinding::Vfio,
            "uio_pci_generic" => DriverBinding::Uio,
            _ => DriverBinding::Unbound,
        }
    }

    fn iommu_groups_present(&self) -> bool {
        let path = format!("{}/sys/kernel/iommu_groups", self.root);
        match std::fs::read_dir(&path) {
            Ok(mut entries) => entries.next().is_some(),
            Err(_) => false,
        }
    }

    fn available_cores(&self) -> usize {
        if let Some(v) = self.cores_override {
            return v;
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

/// Parses a `key:    value` line from `/proc/meminfo`, returning the
/// integer value when the prefix matches. `Hugepagesize` lines carry
/// a trailing `kB` unit; we strip everything after the number.
fn parse_meminfo_field(line: &str, prefix: &str) -> Option<u64> {
    let rest = line.strip_prefix(prefix)?.trim();
    let num_part: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num_part.is_empty() {
        return None;
    }
    num_part.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_root(p: &str) -> SysfsRoot<'_> {
        SysfsRoot {
            root: p,
            hugepages_override_mb: None,
            sys_admin_override: None,
            cores_override: None,
        }
    }

    #[test]
    fn test_parse_meminfo_field_handles_units() {
        assert_eq!(
            parse_meminfo_field("Hugepagesize:      2048 kB", "Hugepagesize:"),
            Some(2048)
        );
        assert_eq!(
            parse_meminfo_field("HugePages_Total:    512", "HugePages_Total:"),
            Some(512)
        );
        assert_eq!(
            parse_meminfo_field("Other:    100", "HugePages_Total:"),
            None
        );
        assert_eq!(
            parse_meminfo_field("HugePages_Total:", "HugePages_Total:"),
            None
        );
    }

    #[test]
    fn test_kernel_version_string_is_non_empty() {
        let v = kernel_version_string();
        assert!(!v.is_empty());
    }

    #[test]
    fn test_io_uring_features_does_not_panic() {
        let _features = io_uring_features();
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_spdk_eligibility_off_linux_returns_not_linux() {
        let e = spdk_eligibility();
        assert!(!e.eligible);
        assert!(e
            .reasons_failed
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::NotLinux)));
        assert!(e.eligible_devices.is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_spdk_eligibility_synthetic_empty_root_reports_no_nvme() {
        let tmp = std::env::temp_dir().join(format!(
            "fsys-cap-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).expect("mktmpdir");
        std::fs::create_dir_all(tmp.join("sys/bus/pci/devices")).expect("mkdevs");
        std::fs::create_dir_all(tmp.join("proc")).expect("mkproc");
        std::fs::write(
            tmp.join("proc/meminfo"),
            "HugePages_Total:        0\nHugepagesize:       2048 kB\n",
        )
        .expect("meminfo");

        let root_str = tmp.to_string_lossy().to_string();
        let root = SysfsRoot {
            root: &root_str,
            hugepages_override_mb: None,
            sys_admin_override: Some(true),
            cores_override: Some(8),
        };
        let e = spdk_eligibility_with_root(root);
        assert!(!e.eligible);
        assert!(e
            .reasons_failed
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::NoNvmeDevices)));
        assert!(e
            .reasons_failed
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::HugepagesNotConfigured { .. })));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_spdk_eligibility_hugepages_threshold_boundary() {
        // Cross-platform: the hugepage threshold check runs identically
        // on every platform because we override the live value. The
        // `NotLinux` short-circuit at the top of the function only
        // triggers when `target_os != linux` — we can't run this
        // boundary check on non-Linux because the function bails early.
        if !cfg!(target_os = "linux") {
            return;
        }
        let root = SysfsRoot {
            root: "/tmp/this-must-not-exist-fsys",
            hugepages_override_mb: Some(SPDK_HUGEPAGE_MIN_MB),
            sys_admin_override: Some(true),
            cores_override: Some(8),
        };
        let e = spdk_eligibility_with_root(root);
        // At exactly the floor, hugepages is *not* flagged.
        assert!(!e
            .reasons_failed
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::HugepagesNotConfigured { .. })));

        let root_below = SysfsRoot {
            root: "/tmp/this-must-not-exist-fsys",
            hugepages_override_mb: Some(SPDK_HUGEPAGE_MIN_MB - 1),
            sys_admin_override: Some(true),
            cores_override: Some(8),
        };
        let e = spdk_eligibility_with_root(root_below);
        assert!(e
            .reasons_failed
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::HugepagesNotConfigured { .. })));
    }

    #[test]
    fn test_spdk_eligibility_core_threshold_boundary() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let mut root = synthetic_root("/tmp/this-must-not-exist-fsys");
        root.hugepages_override_mb = Some(SPDK_HUGEPAGE_RECOMMENDED_MB);
        root.sys_admin_override = Some(true);
        root.cores_override = Some(SPDK_MIN_CORES);
        let e = spdk_eligibility_with_root(root);
        assert!(!e
            .reasons_failed
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::InsufficientCores { .. })));

        let mut root_below = synthetic_root("/tmp/this-must-not-exist-fsys");
        root_below.hugepages_override_mb = Some(SPDK_HUGEPAGE_RECOMMENDED_MB);
        root_below.sys_admin_override = Some(true);
        root_below.cores_override = Some(SPDK_MIN_CORES - 1);
        let e = spdk_eligibility_with_root(root_below);
        assert!(e
            .reasons_failed
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::InsufficientCores { .. })));
    }

    #[test]
    fn test_spdk_eligibility_insufficient_privileges_recorded() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let mut root = synthetic_root("/tmp/this-must-not-exist-fsys");
        root.hugepages_override_mb = Some(SPDK_HUGEPAGE_RECOMMENDED_MB);
        root.sys_admin_override = Some(false);
        root.cores_override = Some(SPDK_MIN_CORES);
        let e = spdk_eligibility_with_root(root);
        assert!(e
            .reasons_failed
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::InsufficientPrivileges)));
    }
}
