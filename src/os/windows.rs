//! Windows-specific OS probes.
//!
//! Real version detection on Windows requires `RtlGetVersion` from
//! `ntdll`, or reading the `CurrentBuildNumber` value from the
//! `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` registry key.
//! Both involve Windows-specific FFI that is out of scope for the
//! foundation layer; both are deferred to `0.0.5`.

#![cfg(target_os = "windows")]

/// Returns the Windows version string.
///
/// Currently always returns `"unknown"`. The real probe
/// (`RtlGetVersion` via `windows-sys`) lands in `0.0.5`.
pub(super) fn probe_version() -> String {
    // TODO(0.0.5): probe RtlGetVersion or the registry.
    "unknown".to_string()
}

/// Returns the distro identifier.
///
/// Windows has no notion of distributions; this always returns `None`.
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
