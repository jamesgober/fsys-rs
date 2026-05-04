//! Per-platform hardware probe implementations.
//!
//! The 0.2.0 foundation shipped stubs that returned conservative defaults
//! for every probed value. 0.5.0 replaces those stubs with real probes
//! against `/sys/`, `IOCTL_STORAGE_*`, IOKit, sysctl, etc., per platform.
//!
//! ## Contract
//!
//! Every probe function in this module **never panics** and **never
//! errors out the handle**. When a probe cannot reach its data source
//! — a sandboxed container has no `/sys/` mount, a Windows process
//! lacks the privileges for `IOCTL_STORAGE_QUERY_PROPERTY`, an macOS
//! sysctl name has gone away — the function logs via the metrics
//! placeholder and returns the documented default
//! ([`crate::hardware::DriveInfo::default`], `MemoryInfo::default`,
//! etc.).
//!
//! This is the "best-effort with honesty" rule from locked decision #3:
//! when we can't determine a value reliably, we return `Unknown` /
//! defaults rather than guessing.
//!
//! ## Process-wide caching
//!
//! Per locked decision #5, the `HardwareInfo` aggregate is cached in a
//! `OnceLock` at the [`crate::hardware`] level. The probe functions
//! themselves run once on first access. Hot-pluggable hardware is **not**
//! detected — fsys assumes the hardware configuration at process startup
//! is the configuration for the entire process lifetime.

#[cfg(target_os = "linux")]
pub(super) mod linux;
#[cfg(target_os = "macos")]
pub(super) mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(super) mod unknown;
#[cfg(target_os = "windows")]
pub(super) mod windows;

#[cfg(target_os = "linux")]
pub(super) use self::linux as platform;
#[cfg(target_os = "macos")]
pub(super) use self::macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(super) use self::unknown as platform;
#[cfg(target_os = "windows")]
pub(super) use self::windows as platform;

/// Tri-state Power Loss Protection status.
///
/// PLP probing is fundamentally unreliable across platforms — the NVMe
/// spec exposes a `Volatile Write Cache Present` bit but says nothing
/// about whether that cache is battery-backed (which is what PLP
/// actually means). Vendor-specific log pages or NVMe-MI 1.1+
/// capabilities are needed for definitive answers.
///
/// Per locked decision #3 in `.dev/DECISIONS-0.5.0.md`, fsys does not
/// guess. When the probe cannot conclusively determine PLP status, the
/// answer is [`PlpStatus::Unknown`].
///
/// **0.5.0 reliability** — the probes return [`PlpStatus::Unknown`]
/// across the board. Definitive PLP detection (via NVMe Identify
/// Controller vendor-specific log pages on Linux, refined IOCTL paths
/// on Windows, IOKit property mining on macOS) lands alongside NVMe
/// passthrough in `0.6.0`. See follow-up F-9 in
/// `.dev/DECISIONS-0.5.0.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum PlpStatus {
    /// PLP is confirmed present.
    Yes,
    /// PLP is confirmed absent.
    No,
    /// PLP status could not be determined.
    #[default]
    Unknown,
}

impl PlpStatus {
    /// Returns a stable lowercase name for this status.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            PlpStatus::Yes => "yes",
            PlpStatus::No => "no",
            PlpStatus::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plp_status_default_is_unknown() {
        assert_eq!(PlpStatus::default(), PlpStatus::Unknown);
    }

    #[test]
    fn test_plp_status_as_str_round_trips() {
        assert_eq!(PlpStatus::Yes.as_str(), "yes");
        assert_eq!(PlpStatus::No.as_str(), "no");
        assert_eq!(PlpStatus::Unknown.as_str(), "unknown");
    }
}
