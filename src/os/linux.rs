//! Linux-specific OS probes.
//!
//! All probes here are deliberately conservative: they read well-known
//! pseudo-files under `/proc` and `/etc`. If the kernel does not expose
//! the file (containers, sandboxes, tmpfs-only roots), the probe returns
//! a documented default rather than failing.

#![cfg(target_os = "linux")]

const OSRELEASE_PATH: &str = "/proc/sys/kernel/osrelease";
const OS_RELEASE_PATH: &str = "/etc/os-release";

/// Reads the running kernel release string from `/proc/sys/kernel/osrelease`.
///
/// Returns `"unknown"` when `/proc` is not mounted or the file cannot be
/// read (e.g. inside a minimal container).
pub(super) fn probe_version() -> String {
    match std::fs::read_to_string(OSRELEASE_PATH) {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                "unknown".to_string()
            } else {
                trimmed.to_string()
            }
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Parses `/etc/os-release` and returns the value of `ID=` (e.g.
/// `"ubuntu"`, `"arch"`, `"debian"`).
///
/// Returns `None` when the file is absent or contains no `ID=` line.
/// Surrounding quotes are stripped per the os-release spec.
pub(super) fn probe_distro() -> Option<String> {
    let content = std::fs::read_to_string(OS_RELEASE_PATH).ok()?;
    for raw in content.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("ID=") {
            let value = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_version_returns_non_empty_string() {
        let v = probe_version();
        assert!(!v.is_empty());
    }

    #[test]
    fn test_probe_distro_returns_either_some_or_none_without_panicking() {
        let _ = probe_distro();
    }
}
