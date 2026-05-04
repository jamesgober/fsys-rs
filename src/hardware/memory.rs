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
/// 0.5.0 delegates to the crate-internal `probe::platform::probe_memory`
/// which reads `/proc/meminfo` (Linux), `GlobalMemoryStatusEx`
/// (Windows), or `sysctlbyname` (macOS). Live, not cached — RAM
/// availability moves constantly. Returns [`MemoryInfo::default`]
/// (zeros) on probe failure (sandbox, missing capability).
#[must_use]
pub(super) fn probe() -> MemoryInfo {
    super::probe::platform::probe_memory()
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
    fn test_probe_returns_non_zero_total_on_real_platforms() {
        // Linux/macOS/Windows produce real values; sandboxed
        // unsupported targets fall back to default.
        let m = probe();
        if cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        )) {
            assert!(
                m.total_bytes > 0,
                "real platforms must report >0 total memory"
            );
        } else {
            assert_eq!(m, MemoryInfo::default());
        }
    }

    #[test]
    fn test_probe_returns_owned_value_each_call() {
        // 0.5.0: probe is live. `total_bytes` is stable across calls,
        // but `available_bytes` drifts as the system runs. Assert
        // that both probes succeed and that total is stable; allow
        // any drift in available.
        let a = probe();
        let b = probe();
        assert_eq!(a.total_bytes, b.total_bytes);
    }
}
