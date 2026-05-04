//! Fallback OS probes used when `target_os` is not Linux, macOS, or
//! Windows.
//!
//! BSDs and exotic targets fall through to this module. The probes
//! return safe defaults so the rest of the crate can rely on
//! [`super::OsInfo`] always being shaped correctly.

#![cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]

/// Returns `"unknown"` — no version probe is available on this target.
pub(super) fn probe_version() -> String {
    "unknown".to_string()
}

/// Returns `None` — distro detection is Linux-specific.
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
