//! macOS-specific OS probes.
//!
//! Real version detection on macOS requires either `sysctl(2)` with
//! `kern.osrelease` / `kern.osproductversion` or a parsed
//! `/System/Library/CoreServices/SystemVersion.plist`. Both pull in
//! platform-specific code that is out of scope for the foundation
//! layer; both are deferred to `0.0.5`.

#![cfg(target_os = "macos")]

/// Returns the macOS version string.
///
/// Currently always returns `"unknown"`. The real probe (sysctl
/// `kern.osproductversion`) lands in `0.0.5`.
pub(super) fn probe_version() -> String {
    // TODO(0.0.5): probe `kern.osproductversion` via sysctlbyname.
    "unknown".to_string()
}

/// Returns the distro identifier.
///
/// macOS has no concept of distributions; this always returns `None`.
pub(super) fn probe_distro() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_version_returns_non_empty_default() {
        assert!(!probe_version().is_empty());
    }

    #[test]
    fn test_probe_distro_returns_none() {
        assert!(probe_distro().is_none());
    }
}
