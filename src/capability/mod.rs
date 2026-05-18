//! System capability detection and on-disk cache (1.1.0).
//!
//! `fsys` 1.0 probed hardware lazily on first access and cached the
//! result in a process-wide `OnceLock`. That worked for the hardware
//! probe — the values were cheap to compute and didn't influence
//! backend selection.
//!
//! 1.1.0 introduces a richer capability surface (SPDK eligibility, IOMMU
//! groups, hugepage configuration, kernel feature flags) that costs
//! tens of milliseconds to probe end-to-end. Re-running that probe on
//! every process start is wasteful; the answer rarely changes between
//! runs on the same machine. This module caches the result to disk
//! and invalidates only when something the answer depends on has
//! changed.
//!
//! ## Cache file location
//!
//! - **Linux / macOS:** `$XDG_CACHE_HOME/fsys/capabilities.toml`,
//!   falling back to `$HOME/.cache/fsys/capabilities.toml`.
//! - **Windows:** `%LOCALAPPDATA%\fsys\capabilities.toml`.
//! - **Override:** set the `FSYS_CACHE_DIR` environment variable to
//!   choose an arbitrary directory (handy for tests and containers).
//!
//! ## Cache invalidation
//!
//! The cache is re-probed when **any** of these are true:
//!
//! 1. The fsys version embedded in the file does not match this
//!    crate's `CARGO_PKG_VERSION`.
//! 2. The kernel version (Linux/macOS) or build number (Windows) in
//!    the file does not match the live value.
//! 3. The schema version in the file does not match
//!    [`Capabilities::SCHEMA_VERSION`].
//! 4. The cache file's modification time is older than 30 days.
//! 5. The `FSYS_REPROBE` environment variable is set to `1`.
//! 6. The cache file is missing, unreadable, or fails to parse.
//!
//! ## Public API
//!
//! - [`capabilities()`] — returns the cached snapshot (or runs the
//!   probe + writes the cache on first call). Sub-millisecond on
//!   cache hit.
//! - [`probe_capabilities_fresh()`] — forces a re-probe, ignoring the
//!   cache. Writes the new result to the cache file.
//! - [`invalidate_capability_cache()`] — deletes the cache file so
//!   the next [`capabilities()`] call re-probes.
//!
//! ## What lives here vs. [`crate::hardware`]
//!
//! `hardware` is the foundational probe used by every fsys handle —
//! drive identity, CPU features, IO primitive availability. Those
//! probes are cheap (microseconds) and run in-process; no cache
//! file is justified.
//!
//! `capability` is the *user-space backend eligibility* layer that
//! 1.1.0 builds on top: which optional backends (SPDK, future
//! PMEM / RDMA) the system can host. These probes are expensive
//! (`/sys/bus/pci/...` walk, `/proc/meminfo` parse, IOMMU group
//! enumeration) and the answer is stable across runs, so they cache
//! to disk. Internally, [`Capabilities`] re-exports the relevant
//! subset of [`crate::hardware::HardwareInfo`] for convenience.

pub mod cache;
pub mod probe;
mod toml_lite;
pub mod types;

pub use types::{
    Capabilities, HardwareSummary, IoUringFeature, PciAddress, SpdkEligibility, SpdkSkipReason,
};

use std::sync::OnceLock;

/// Schema version embedded in the on-disk cache file.
///
/// Bumped whenever the cache file's serialised shape changes in a
/// way that requires re-probing. Bumping this constant forces every
/// existing cache to be invalidated on the next [`capabilities()`]
/// call, regardless of whether the kernel version, fsys version, or
/// age would have triggered re-probing on their own.
pub const CAPABILITY_CACHE_SCHEMA_VERSION: u32 = 1;

/// Cache file age threshold beyond which the entry is considered stale.
///
/// 30 days. Hardware and OS state rarely change inside this window
/// on a server; CI runners may re-probe more often via
/// `FSYS_REPROBE=1`.
pub const CAPABILITY_CACHE_MAX_AGE_DAYS: u64 = 30;

/// Process-wide cache. First call to [`capabilities()`] populates it
/// (reading the disk cache when valid, running the probe + writing
/// the disk cache otherwise). Subsequent calls return the same
/// reference.
static CAPABILITIES: OnceLock<Capabilities> = OnceLock::new();

/// Returns the cached system capability snapshot.
///
/// The first call after process start may take 50-200 ms (full
/// probe). Subsequent calls return a borrowed reference to the same
/// data and complete in well under one millisecond.
///
/// The probe **never panics**. Sub-probes that fail (missing
/// `/proc/meminfo`, denied PCI enumeration, etc.) record their
/// failure as a [`SpdkSkipReason`] on the returned snapshot; the
/// rest of the crate continues to operate using whatever data the
/// probe did manage to collect.
///
/// # Examples
///
/// ```
/// let caps = fsys::capability::capabilities();
/// // Every fsys host has at least conservative defaults.
/// let _hw = &caps.hardware;
/// ```
#[must_use]
pub fn capabilities() -> &'static Capabilities {
    CAPABILITIES.get_or_init(|| {
        let snapshot = if std::env::var("FSYS_REPROBE").as_deref() == Ok("1") {
            probe_fresh()
        } else {
            match cache::load() {
                Ok(Some(c)) => c,
                Ok(None) | Err(_) => probe_fresh(),
            }
        };
        // Best-effort write. Failure to persist the cache is not fatal;
        // it just means the next process will re-probe. Errors are
        // ignored deliberately — REPS forbids silent error swallow,
        // but this is the documented "best-effort persistence" path.
        let _ = cache::store(&snapshot);
        snapshot
    })
}

/// Runs a fresh probe, ignoring any cached data on disk.
///
/// Updates the on-disk cache file with the new result on success.
/// Use this from diagnostic tooling, after a system reconfiguration
/// (e.g. just enabled IOMMU in the kernel command line), or whenever
/// you specifically want the live answer rather than the cached one.
///
/// Note: the process-wide [`OnceLock`] returned by [`capabilities()`]
/// is **not** updated by this call. The next process start is when
/// the cached snapshot is rebuilt.
#[must_use]
pub fn probe_capabilities_fresh() -> Capabilities {
    let fresh = probe_fresh();
    let _ = cache::store(&fresh);
    fresh
}

/// Deletes the on-disk capability cache file.
///
/// The next call to [`capabilities()`] from a new process will
/// re-probe and rewrite the cache. Returns `Ok(())` whether the
/// file existed or not (deleting a non-existent file is treated
/// as success).
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the file existed
/// but could not be deleted (permissions, hardware error, etc.).
pub fn invalidate_capability_cache() -> std::io::Result<()> {
    cache::invalidate()
}

/// Runs the full probe — no cache, no shortcuts. Called by
/// [`capabilities()`] on first access and by
/// [`probe_capabilities_fresh()`] explicitly.
fn probe_fresh() -> Capabilities {
    let hardware = HardwareSummary::from_live_probe();
    let io_uring_features = probe::io_uring_features();
    let spdk_elig = probe::spdk_eligibility();
    Capabilities {
        schema_version: CAPABILITY_CACHE_SCHEMA_VERSION,
        fsys_version: env!("CARGO_PKG_VERSION").to_string(),
        kernel_version: probe::kernel_version_string(),
        os_target: std::env::consts::OS.to_string(),
        probed_at_unix_secs: now_unix_secs(),
        io_uring: hardware.io_uring,
        io_uring_features,
        nvme_passthrough: hardware.nvme_passthrough,
        direct_io: hardware.direct_io,
        plp_detected: hardware.plp_detected,
        spdk_eligible: spdk_elig.eligible,
        spdk_skip_reasons: spdk_elig.reasons_failed,
        spdk_eligible_devices: spdk_elig.eligible_devices,
        hardware,
    }
}

/// Returns the current wall-clock time as seconds since the Unix
/// epoch. Uses [`SystemTime::now`] internally; clamps to 0 if the
/// system clock is set before 1970 (degenerate but defensible).
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::cache::TEST_ENV_LOCK;

    /// Acquires the shared cache test lock. Use this from any test
    /// that calls `capabilities()` / `probe_capabilities_fresh()`
    /// or otherwise touches the on-disk cache, so concurrent
    /// `capability::cache::tests` don't race against us on
    /// `FSYS_CACHE_DIR`.
    fn cache_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn test_capabilities_returns_consistent_reference() {
        let _g = cache_lock();
        let a = capabilities() as *const Capabilities;
        let b = capabilities() as *const Capabilities;
        assert_eq!(a, b);
    }

    #[test]
    fn test_capabilities_fsys_version_matches_cargo() {
        let _g = cache_lock();
        let caps = capabilities();
        assert_eq!(caps.fsys_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn test_capabilities_schema_version_matches_constant() {
        let _g = cache_lock();
        let caps = capabilities();
        assert_eq!(caps.schema_version, CAPABILITY_CACHE_SCHEMA_VERSION);
    }

    #[test]
    fn test_capabilities_os_target_matches_const() {
        let _g = cache_lock();
        let caps = capabilities();
        assert_eq!(caps.os_target, std::env::consts::OS);
    }

    #[test]
    fn test_capabilities_probed_at_is_positive() {
        let _g = cache_lock();
        let caps = capabilities();
        // A modern build's probe should always produce a 21st-century timestamp.
        assert!(caps.probed_at_unix_secs > 1_577_836_800); // 2020-01-01
    }

    #[test]
    fn test_probe_capabilities_fresh_returns_owned_snapshot() {
        let _g = cache_lock();
        let a = probe_capabilities_fresh();
        let b = probe_capabilities_fresh();
        // Same shape, no shared references — these are owned values.
        assert_eq!(a.schema_version, b.schema_version);
        assert_eq!(a.fsys_version, b.fsys_version);
        assert_eq!(a.os_target, b.os_target);
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_capabilities_spdk_eligible_false_off_linux() {
        let _g = cache_lock();
        let caps = capabilities();
        assert!(!caps.spdk_eligible);
        assert!(caps
            .spdk_skip_reasons
            .iter()
            .any(|r| matches!(r, SpdkSkipReason::NotLinux)));
    }
}
