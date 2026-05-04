//! System-memory probe.
//!
//! Real probing requires platform-specific syscalls
//! (`sysinfo` / `/proc/meminfo` on Linux, `GlobalMemoryStatusEx` on
//! Windows, `host_statistics64` on macOS). All of those are deferred
//! to `0.0.5`. The `0.0.2` foundation returns a [`MemoryInfo`]
//! populated with `0` totals so the type is shaped correctly without
//! lying about real values.

/// Snapshot of system memory.
///
/// Both fields are in bytes. `total_bytes == 0` means the foundation
/// stub has not yet been replaced; treat any zero as "unknown" rather
/// than "no memory".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryInfo {
    /// Total physical memory in bytes. `0` while the probe is stubbed.
    pub total_bytes: u64,
    /// Available memory in bytes (free + reclaimable). `0` while
    /// stubbed.
    pub available_bytes: u64,
}

/// Returns a fresh [`MemoryInfo`] snapshot.
///
/// Always returns the `Default` value in `0.0.2`. Real probing (live,
/// platform-specific) lands in `0.0.5`.
#[must_use]
pub(super) fn probe() -> MemoryInfo {
    // TODO(0.0.5): replace with real per-platform probes.
    //  - Linux: parse /proc/meminfo (MemTotal, MemAvailable).
    //  - Windows: GlobalMemoryStatusEx().
    //  - macOS: host_statistics64() with HOST_VM_INFO64.
    MemoryInfo::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_total_bytes_is_zero() {
        assert_eq!(MemoryInfo::default().total_bytes, 0);
    }

    #[test]
    fn test_default_available_bytes_is_zero() {
        assert_eq!(MemoryInfo::default().available_bytes, 0);
    }

    #[test]
    fn test_probe_matches_default_in_foundation_phase() {
        assert_eq!(probe(), MemoryInfo::default());
    }

    #[test]
    fn test_probe_returns_owned_value_each_call() {
        let a = probe();
        let b = probe();
        assert_eq!(a, b);
    }
}
