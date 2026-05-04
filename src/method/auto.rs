//! `Method::Auto` resolution ladder — real-probe-aware (0.5.0).
//!
//! 0.3.0 shipped a heuristic ladder that didn't actually consult
//! hardware data (it just returned `Direct` on macOS / Windows
//! unconditionally and only used `direct_io` capability sniffing on
//! Linux). 0.5.0 replaces that with a ladder that reads
//! [`crate::hardware::info`] — the cached real probe — and picks the
//! method that is safe + fast for the actual hardware + OS combination.
//!
//! Decision matrix locked in `.dev/PROMPTS/0.5.0-methods.md` and
//! restated in `Method::Auto`'s doc comment.

use super::Method;

/// Resolves `Method::Auto` to a concrete method using real probe data.
///
/// Always returns one of `Method::Sync`, `Method::Data`, or
/// `Method::Direct`. Never returns `Method::Auto`. `Method::Mmap` is
/// not selected by `Auto` — Mmap is a deliberate caller choice for
/// read-heavy random-access workloads, not a default. `Method::Journal`
/// is reserved for 0.7.0.
#[must_use]
pub(super) fn resolve_auto() -> Method {
    use crate::hardware;

    let io = hardware::io_primitives();
    let drive = hardware::drive();

    resolve_auto_inner(drive.kind, io.io_uring)
}

/// Inner ladder, parameterised on the inputs so it can be unit-tested
/// with synthetic combinations rather than only the live probe result.
#[cfg(target_os = "linux")]
fn resolve_auto_inner(kind: crate::hardware::drive::DriveKind, io_uring_available: bool) -> Method {
    use crate::hardware::drive::DriveKind;
    match kind {
        DriveKind::Nvme if io_uring_available => Method::Direct,
        DriveKind::Nvme => Method::Data,
        DriveKind::SataSsd => Method::Data,
        DriveKind::Hdd | DriveKind::Unknown => Method::Sync,
    }
}

#[cfg(target_os = "macos")]
fn resolve_auto_inner(
    kind: crate::hardware::drive::DriveKind,
    _io_uring_available: bool,
) -> Method {
    use crate::hardware::drive::DriveKind;
    // macOS NVMe detection requires IOKit (deferred to 0.6.0).
    // Until then, the macOS probe always returns `Unknown` for
    // `DriveKind`, which lands here in the catch-all → `Sync`.
    // When IOKit lands and NVMe becomes detectable, the `Nvme` arm
    // activates; SSD/HDD/Unknown remain on `Sync` because macOS's
    // F_NOCACHE is not always a win on non-NVMe.
    match kind {
        DriveKind::Nvme => Method::Direct,
        DriveKind::SataSsd | DriveKind::Hdd | DriveKind::Unknown => Method::Sync,
    }
}

#[cfg(target_os = "windows")]
fn resolve_auto_inner(
    kind: crate::hardware::drive::DriveKind,
    _io_uring_available: bool,
) -> Method {
    use crate::hardware::drive::DriveKind;
    // Windows NVMe vs SSD vs HDD detection requires
    // IOCTL_STORAGE_QUERY_PROPERTY with StorageDeviceSeekPenaltyProperty
    // (deferred to 0.6.0). Until then, the Windows probe always returns
    // `Unknown`, which lands here in the catch-all → `Sync`.
    // When IOCTL probing lands, Nvme + SataSsd activate the Direct arm;
    // HDD and Unknown remain on `Sync`.
    match kind {
        DriveKind::Nvme | DriveKind::SataSsd => Method::Direct,
        DriveKind::Hdd | DriveKind::Unknown => Method::Sync,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn resolve_auto_inner(
    _kind: crate::hardware::drive::DriveKind,
    _io_uring_available: bool,
) -> Method {
    // Universal safe fallback. fsync exists on every POSIX system.
    Method::Sync
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::drive::DriveKind;

    #[test]
    fn test_resolve_auto_returns_concrete() {
        let r = resolve_auto();
        assert!(
            matches!(r, Method::Sync | Method::Data | Method::Direct),
            "Auto must resolve to Sync/Data/Direct; got {:?}",
            r
        );
    }

    // ── Linux ladder unit tests (all platforms compile these but
    //    the inner is cfg-gated; only the active platform's `inner`
    //    is in scope) ────────────────────────────────────────────

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_nvme_with_io_uring_picks_direct() {
        assert_eq!(resolve_auto_inner(DriveKind::Nvme, true), Method::Direct);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_nvme_without_io_uring_picks_data() {
        assert_eq!(resolve_auto_inner(DriveKind::Nvme, false), Method::Data);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_sata_ssd_picks_data() {
        assert_eq!(resolve_auto_inner(DriveKind::SataSsd, true), Method::Data);
        assert_eq!(resolve_auto_inner(DriveKind::SataSsd, false), Method::Data);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_hdd_picks_sync() {
        assert_eq!(resolve_auto_inner(DriveKind::Hdd, true), Method::Sync);
        assert_eq!(resolve_auto_inner(DriveKind::Hdd, false), Method::Sync);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_unknown_picks_sync() {
        assert_eq!(resolve_auto_inner(DriveKind::Unknown, true), Method::Sync);
        assert_eq!(resolve_auto_inner(DriveKind::Unknown, false), Method::Sync);
    }

    // ── macOS ladder unit tests ────────────────────────────────────

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_nvme_picks_direct() {
        assert_eq!(resolve_auto_inner(DriveKind::Nvme, false), Method::Direct);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_non_nvme_picks_sync() {
        assert_eq!(resolve_auto_inner(DriveKind::SataSsd, false), Method::Sync);
        assert_eq!(resolve_auto_inner(DriveKind::Hdd, false), Method::Sync);
        assert_eq!(resolve_auto_inner(DriveKind::Unknown, false), Method::Sync);
    }

    // ── Windows ladder unit tests ──────────────────────────────────

    #[test]
    #[cfg(target_os = "windows")]
    fn test_windows_nvme_picks_direct() {
        assert_eq!(resolve_auto_inner(DriveKind::Nvme, false), Method::Direct);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_windows_sata_ssd_picks_direct() {
        assert_eq!(
            resolve_auto_inner(DriveKind::SataSsd, false),
            Method::Direct
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_windows_hdd_or_unknown_picks_sync() {
        assert_eq!(resolve_auto_inner(DriveKind::Hdd, false), Method::Sync);
        assert_eq!(resolve_auto_inner(DriveKind::Unknown, false), Method::Sync);
    }
}
