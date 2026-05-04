//! Durability strategy enum and automatic hardware-aware selection.
//!
//! [`Method`] is the central control knob for fsys. It determines which OS
//! synchronisation primitive runs after every write, and therefore the
//! trade-off between throughput and crash-safety. The correct choice depends
//! on the hardware, the application's durability requirements, and the
//! filesystem in use. When in doubt, use [`Method::Auto`].
//!
//! # Platform-specific notes
//!
//! See the per-variant documentation for exact OS primitives. The high-level
//! story: Linux has the widest selection (`fdatasync`, `O_DIRECT`,
//! `io_uring` since 0.5.0), macOS requires `F_FULLFSYNC` for any
//! meaningful durability guarantee, and Windows uses `FlushFileBuffers`
//! with optional `FILE_FLAG_NO_BUFFERING`.
//!
//! # Module structure
//!
//! - This file (`mod.rs`) defines the `Method` enum, its `to_u8` /
//!   `from_u8` / `is_reserved` / `as_str` / `Display` impls, and the
//!   public `resolve()` entry point that delegates to the
//!   crate-internal `auto` submodule.
//! - The `auto` submodule contains the per-platform `resolve_auto`
//!   ladder. 0.5.0 replaces 0.3.0's heuristic ladder with one that
//!   consults the real hardware probe (D-4 in
//!   `.dev/DECISIONS-0.5.0.md`).
//! - Per-method backends (`sync`, `data`, `direct`, `mmap`, `journal`)
//!   land in their own files as they are implemented. The
//!   `mmap` and `direct` upgrades arrive in checkpoints E and F+G of
//!   the 0.5.0 phase respectively.

mod auto;
pub(crate) mod mmap;

use std::fmt;

/// Durability strategy for file IO operations.
///
/// The variant controls which OS synchronisation primitive is invoked after
/// every write. `Sync`, `Data`, `Direct`, and `Auto` are fully functional
/// in `0.4.0` and earlier; `Mmap` becomes a real backend in `0.5.0`.
/// `Journal` is reserved for `0.7.0`.
///
/// # Platform-specific behavior
///
/// | Variant | Linux | macOS | Windows |
/// |---------|-------|-------|---------|
/// | `Sync`  | `fsync(2)` | `fcntl(F_FULLFSYNC)` | `FlushFileBuffers` |
/// | `Data`  | `fdatasync(2)` | `F_FULLFSYNC` (fallback) | `FlushFileBuffers` (fallback) |
/// | `Direct`| `O_DIRECT` + `io_uring` (0.5.0) / `fdatasync` (fallback) | `F_NOCACHE` + `F_FULLFSYNC` | `FILE_FLAG_NO_BUFFERING\|WRITE_THROUGH` |
/// | `Mmap`  | `mmap` + `msync(MS_SYNC)` (0.5.0) | `mmap` + `msync(MS_SYNC)` (0.5.0) | `MapViewOfFile` + `FlushViewOfFile` (0.5.0) |
/// | `Journal`| *reserved* | *reserved* | *reserved* |
/// | `Auto`  | hardware ladder (real probe in 0.5.0) | hardware ladder | hardware ladder |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum Method {
    /// Standard full-file synchronisation.
    ///
    /// Guarantees that the file's data and metadata are on stable media
    /// before the call returns.
    ///
    /// # Platform-specific behavior
    ///
    /// - **Linux:** `fsync(2)`.
    /// - **macOS:** `fcntl(fd, F_FULLFSYNC, 0)`. **Not** `fsync(2)` — on
    ///   macOS, regular `fsync` stops at the drive's write cache and does
    ///   NOT guarantee media durability. `F_FULLFSYNC` is the only correct
    ///   primitive for crash-safe writes on macOS.
    /// - **Windows:** `FlushFileBuffers(handle)`.
    Sync = 0,

    /// Data-only synchronisation; skips non-critical metadata where safe.
    ///
    /// Faster than [`Sync`](Method::Sync) on Linux when only data
    /// durability matters and the file size has not changed (no inode
    /// metadata update is required for `mtime` to be correct on recovery).
    ///
    /// # Platform-specific behavior
    ///
    /// - **Linux:** `fdatasync(2)`.
    /// - **macOS:** Falls back to [`Sync`](Method::Sync) (`F_FULLFSYNC`).
    ///   macOS has no `fdatasync` equivalent. `active_method()` will
    ///   reflect `Sync` after this fallback.
    /// - **Windows:** Falls back to [`Sync`](Method::Sync)
    ///   (`FlushFileBuffers`). Windows has no `fdatasync` equivalent.
    ///   `active_method()` will reflect `Sync` after this fallback.
    Data = 1,

    /// Direct IO — bypasses the OS page cache entirely.
    ///
    /// The application owns cache management. Reads and writes go
    /// directly to or from the storage device without passing through
    /// the kernel page cache. Buffer alignment to the logical sector
    /// size is handled internally; callers pass arbitrary byte slices.
    ///
    /// Falls back to [`Data`](Method::Data) then [`Sync`](Method::Sync)
    /// when the filesystem rejects Direct IO (e.g. tmpfs, some FUSE
    /// mounts). The fallback is observable via
    /// [`Handle::active_method`](crate::Handle::active_method).
    ///
    /// # Platform-specific behavior
    ///
    /// - **Linux (0.5.0):** Files opened with `O_DIRECT`. IO submission
    ///   via `io_uring` when the kernel supports it (5.1+); fallback to
    ///   `pwrite(2)` + `fdatasync(2)` when `io_uring_setup` fails.
    ///   Buffer + offset + length alignment to `logical_sector` (typically
    ///   512 or 4096 bytes) is handled by the per-handle aligned buffer
    ///   pool.
    /// - **macOS:** `fcntl(fd, F_NOCACHE, 1)` after open. Durability via
    ///   `fcntl(fd, F_FULLFSYNC, 0)`. If `F_NOCACHE` fails (rare on some
    ///   HFS+ configurations), falls back to `Sync`.
    /// - **Windows:** `CreateFileW` with
    ///   `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`. Sector
    ///   alignment is probed at handle creation via `GetDiskFreeSpaceW`.
    Direct = 2,

    /// Memory-mapped IO with `msync` for durability.
    ///
    /// Reads come from a mapped region; writes flow through the mapping
    /// with explicit `msync(MS_SYNC)` (Linux/macOS) or `FlushViewOfFile`
    /// (Windows) for durability. Falls back to [`Method::Sync`] for
    /// files smaller than the page size, special files (sockets, pipes,
    /// FIFOs), and filesystems that reject `mmap`. The fallback is
    /// observable via
    /// [`Handle::active_method`](crate::Handle::active_method).
    ///
    /// **Real backend lands in `0.5.0`.** Earlier versions return
    /// [`Error::UnsupportedMethod`](crate::Error::UnsupportedMethod) at
    /// runtime.
    Mmap = 3,

    /// Intent-log (journal) durability mode.
    ///
    /// **Reserved for `0.7.0`.** Selecting this method returns
    /// [`Error::UnsupportedMethod`](crate::Error::UnsupportedMethod) at
    /// runtime.
    Journal = 4,

    /// Hardware-aware automatic selection.
    ///
    /// Probes the drive kind and available IO primitives once at
    /// [`Handle`](crate::Handle) creation, then picks the fastest method
    /// that is safe on the current hardware and OS. The resolved concrete
    /// method is visible via
    /// [`Handle::active_method`](crate::Handle::active_method) (which
    /// never returns `Auto`).
    ///
    /// # Resolution ladder (0.5.0)
    ///
    /// 0.5.0 replaces 0.3.0's heuristic ladder with one that consults
    /// real probe data ([`crate::hardware::info`]). See the
    /// crate-internal `auto` module and the 0.5.0 prompt's Auto table
    /// for the full matrix.
    ///
    /// | Condition | Resolves to |
    /// |---|---|
    /// | Linux + io_uring + NVMe | `Direct` |
    /// | Linux + NVMe without io_uring | `Data` |
    /// | Linux + SSD | `Data` |
    /// | Linux + HDD or Unknown | `Sync` |
    /// | macOS + NVMe | `Direct` |
    /// | macOS + non-NVMe SSD or Unknown | `Sync` |
    /// | macOS + HDD | `Sync` |
    /// | Windows + NVMe | `Direct` |
    /// | Windows + SSD | `Direct` |
    /// | Windows + HDD or Unknown | `Sync` |
    /// | Hardware probe failed entirely | `Sync` (universal safety) |
    ///
    /// PLP is **not** consulted by the 0.5.0 ladder — the elite
    /// NVMe-passthrough path that benefits most from PLP is deferred to
    /// `0.6.0`.
    Auto = 5,
}

impl Method {
    /// Converts the method to its raw `u8` discriminant for atomic storage.
    #[must_use]
    #[inline]
    pub(crate) const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Converts a raw `u8` value back to a [`Method`].
    ///
    /// Returns [`Method::Sync`] for any unrecognised value as a defensive
    /// fallback — `Sync` is always available on every platform.
    #[must_use]
    #[inline]
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => Method::Sync,
            1 => Method::Data,
            2 => Method::Direct,
            3 => Method::Mmap,
            4 => Method::Journal,
            5 => Method::Auto,
            _ => Method::Sync,
        }
    }

    /// Returns `true` for reserved variants not yet implemented.
    ///
    /// In 0.5.0 the only reserved variant is [`Method::Journal`]
    /// (deferred to 0.7.0). [`Method::Mmap`] is no longer reserved —
    /// it ships with a real backend in this phase.
    #[must_use]
    #[inline]
    pub const fn is_reserved(self) -> bool {
        matches!(self, Method::Journal)
    }

    /// Resolves [`Auto`](Method::Auto) to a concrete method using real
    /// hardware probe data (0.5.0).
    ///
    /// Inspects the cached [`crate::hardware::HardwareInfo`] (drive
    /// kind plus IO-primitives availability) and the active platform,
    /// then picks the fastest method that is safe on this system.
    /// Always returns a concrete variant — never
    /// [`Auto`](Method::Auto). Non-`Auto` methods are returned
    /// unchanged.
    ///
    /// See [`Method::Auto`] for the full resolution ladder.
    #[must_use]
    pub fn resolve(self) -> Method {
        if self != Method::Auto {
            return self;
        }
        auto::resolve_auto()
    }

    /// Returns the canonical lowercase name for this method.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Sync => "sync",
            Method::Data => "data",
            Method::Direct => "direct",
            Method::Mmap => "mmap",
            Method::Journal => "journal",
            Method::Auto => "auto",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_method_sync_as_str() {
        assert_eq!(Method::Sync.as_str(), "sync");
    }

    #[test]
    fn test_method_auto_as_str() {
        assert_eq!(Method::Auto.as_str(), "auto");
    }

    #[test]
    fn test_method_display_matches_as_str() {
        for m in [
            Method::Sync,
            Method::Data,
            Method::Direct,
            Method::Mmap,
            Method::Journal,
            Method::Auto,
        ] {
            assert_eq!(m.to_string(), m.as_str());
        }
    }

    #[test]
    fn test_method_to_u8_and_back() {
        for m in [
            Method::Sync,
            Method::Data,
            Method::Direct,
            Method::Mmap,
            Method::Journal,
            Method::Auto,
        ] {
            assert_eq!(Method::from_u8(m.to_u8()), m);
        }
    }

    #[test]
    fn test_method_from_u8_invalid_returns_sync() {
        assert_eq!(Method::from_u8(200), Method::Sync);
    }

    #[test]
    fn test_method_is_reserved_true_for_journal_only() {
        // 0.5.0: Mmap is no longer reserved.
        assert!(Method::Journal.is_reserved());
        assert!(!Method::Mmap.is_reserved());
    }

    #[test]
    fn test_method_is_reserved_false_for_real_methods() {
        for m in [
            Method::Sync,
            Method::Data,
            Method::Direct,
            Method::Mmap,
            Method::Auto,
        ] {
            assert!(!m.is_reserved(), "{} should not be reserved", m);
        }
    }

    #[test]
    fn test_method_resolve_passes_through_non_auto() {
        assert_eq!(Method::Sync.resolve(), Method::Sync);
        assert_eq!(Method::Data.resolve(), Method::Data);
        assert_eq!(Method::Direct.resolve(), Method::Direct);
        assert_eq!(Method::Mmap.resolve(), Method::Mmap);
    }

    #[test]
    fn test_method_auto_resolves_to_concrete_variant() {
        let resolved = Method::Auto.resolve();
        assert_ne!(
            resolved,
            Method::Auto,
            "Auto must resolve to a concrete method"
        );
    }

    #[test]
    fn test_method_auto_resolves_to_known_real_variant() {
        // 0.5.0: Auto only resolves to real-backend variants
        // (Sync, Data, Direct). Mmap is real but Auto does not pick
        // it — Mmap is a deliberate caller choice for read-heavy
        // random access workloads, not a default.
        let resolved = Method::Auto.resolve();
        assert!(
            matches!(resolved, Method::Sync | Method::Data | Method::Direct),
            "Auto must resolve to Sync, Data, or Direct; got {:?}",
            resolved
        );
    }
}
