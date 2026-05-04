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
//! story: Linux has the widest selection (`fdatasync`, `O_DIRECT`), macOS
//! requires `F_FULLFSYNC` for any meaningful durability guarantee, and
//! Windows uses `FlushFileBuffers` with optional `FILE_FLAG_NO_BUFFERING`.

use std::fmt;

/// Durability strategy for file IO operations.
///
/// The variant controls which OS synchronisation primitive is invoked after
/// every write. `Sync`, `Data`, `Direct`, and `Auto` are fully functional in
/// `0.3.0`. `Mmap` and `Journal` are reserved variants — selecting them at
/// runtime returns [`crate::Error::UnsupportedMethod`].
///
/// # Platform-specific behavior
///
/// | Variant | Linux | macOS | Windows |
/// |---------|-------|-------|---------|
/// | `Sync`  | `fsync(2)` | `fcntl(F_FULLFSYNC)` | `FlushFileBuffers` |
/// | `Data`  | `fdatasync(2)` | `F_FULLFSYNC` (fallback) | `FlushFileBuffers` (fallback) |
/// | `Direct`| `O_DIRECT` + `fdatasync` | `F_NOCACHE` + `F_FULLFSYNC` | `FILE_FLAG_NO_BUFFERING\|WRITE_THROUGH` |
/// | `Mmap`  | *reserved* | *reserved* | *reserved* |
/// | `Journal`| *reserved* | *reserved* | *reserved* |
/// | `Auto`  | hardware ladder | hardware ladder | hardware ladder |
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
    /// - **Linux:** Files opened with `O_DIRECT`. After write,
    ///   `fdatasync(2)` is called because `O_DIRECT` only bypasses the
    ///   page cache — file-size metadata still needs to be flushed.
    ///   Buffer and offset alignment to `logical_sector` (typically 512
    ///   or 4096 bytes) is handled with a heap-allocated aligned scratch
    ///   buffer when the caller's data is not already aligned.
    /// - **macOS:** `fcntl(fd, F_NOCACHE, 1)` after open. Durability via
    ///   `fcntl(fd, F_FULLFSYNC, 0)`. If `F_NOCACHE` fails (rare on some
    ///   HFS+ configurations), falls back to `Sync`.
    /// - **Windows:** `CreateFileW` with
    ///   `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`. Sector
    ///   alignment is probed at handle creation via `GetDiskFreeSpaceW`
    ///   and handled with an aligned scratch buffer.
    Direct = 2,

    /// Memory-mapped IO with `msync` for durability.
    ///
    /// **Reserved for `0.5.0`.** Selecting this method returns
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
    /// # Resolution ladder (0.3.0)
    ///
    /// | Condition | Resolves to |
    /// |-----------|-------------|
    /// | Linux + `direct_io` capability flag + NVMe/SSD/Unknown drive | `Direct` |
    /// | Linux + HDD or `direct_io` not set | `Data` |
    /// | macOS | `Direct` (attempts `F_NOCACHE`; falls back at runtime) |
    /// | Windows | `Direct` (attempts `NO_BUFFERING`; falls back at runtime) |
    /// | Unknown platform | `Sync` |
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
    /// Calling any IO operation with a reserved method will return
    /// [`Error::UnsupportedMethod`](crate::Error::UnsupportedMethod).
    #[must_use]
    #[inline]
    pub const fn is_reserved(self) -> bool {
        matches!(self, Method::Mmap | Method::Journal)
    }

    /// Resolves [`Auto`](Method::Auto) to a concrete method.
    ///
    /// Inspects the hardware probe results and platform capabilities, then
    /// returns the fastest method that is safe on this system. Always
    /// returns a concrete variant — never [`Auto`](Method::Auto).
    ///
    /// Non-`Auto` methods are returned unchanged.
    ///
    /// # Platform-specific behavior
    ///
    /// - **Linux:** `Direct` when `direct_io` is available and the drive
    ///   is NVMe, SSD, or `Unknown` (treated as SSD-class in `0.3.0`).
    ///   `Data` otherwise.
    /// - **macOS:** `Direct` (falls back to `Sync` at runtime if
    ///   `F_NOCACHE` is unavailable).
    /// - **Windows:** `Direct` (falls back to `Sync` at runtime if
    ///   `NO_BUFFERING` is rejected).
    /// - **Other platforms:** `Sync`.
    #[must_use]
    pub fn resolve(self) -> Method {
        if self != Method::Auto {
            return self;
        }
        resolve_auto()
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
// Auto-resolution ladder — compiled per-platform.
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn resolve_auto() -> Method {
    use crate::hardware;
    use crate::hardware::drive::DriveKind;

    let io = hardware::io_primitives();
    let drive = hardware::drive();

    if io.direct_io {
        // direct_io flag means the kernel exposes O_DIRECT.
        // In 0.3.0 we treat Unknown drives as SSD-class (conservative but
        // not overly conservative — nearly all modern hardware benefits from
        // O_DIRECT). Real NVMe identification lands in 0.5.0.
        //
        // `DriveKind` is `#[non_exhaustive]`. We deliberately match every
        // current variant explicitly rather than adding a `_` wildcard:
        // when 0.5.0 adds a new variant, this match will fail to compile,
        // forcing a contributor to consciously decide which `Method` is
        // appropriate for the new drive class. Adding a `_` arm would
        // silently default the new variant to `Data`, which is a
        // correctness hazard, not a feature.
        match drive.kind {
            DriveKind::Nvme | DriveKind::SataSsd | DriveKind::Unknown => {
                return Method::Direct;
            }
            DriveKind::Hdd => {
                // HDD: O_DIRECT offers no throughput benefit and can hurt
                // sequential performance. Fall through to Data.
            }
        }
    }

    // fdatasync(2) is always available on Linux ≥ 2.4.
    Method::Data
}

#[cfg(target_os = "macos")]
fn resolve_auto() -> Method {
    // macOS: attempt F_NOCACHE at open time. If it fails (rare), the
    // per-operation fallback path in crud/file.rs will downgrade to Sync
    // and update active_method() accordingly.
    Method::Direct
}

#[cfg(target_os = "windows")]
fn resolve_auto() -> Method {
    // Windows: attempt FILE_FLAG_NO_BUFFERING at open time. CreateFileW
    // will return ERROR_INVALID_PARAMETER on filesystems that reject it
    // (e.g. FAT16, some remote mounts), at which point the per-operation
    // fallback path will downgrade to Sync.
    Method::Direct
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn resolve_auto() -> Method {
    // Unknown platform: universal safe fallback. fsync is available on
    // every POSIX system but we do not make specific assumptions here.
    Method::Sync
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
    fn test_method_is_reserved_true_for_mmap_and_journal() {
        assert!(Method::Mmap.is_reserved());
        assert!(Method::Journal.is_reserved());
    }

    #[test]
    fn test_method_is_reserved_false_for_real_methods() {
        for m in [Method::Sync, Method::Data, Method::Direct, Method::Auto] {
            assert!(!m.is_reserved(), "{} should not be reserved", m);
        }
    }

    #[test]
    fn test_method_resolve_passes_through_non_auto() {
        assert_eq!(Method::Sync.resolve(), Method::Sync);
        assert_eq!(Method::Data.resolve(), Method::Data);
        assert_eq!(Method::Direct.resolve(), Method::Direct);
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
    fn test_method_auto_resolves_to_known_variant() {
        let resolved = Method::Auto.resolve();
        assert!(
            matches!(resolved, Method::Sync | Method::Data | Method::Direct),
            "Auto must resolve to Sync, Data, or Direct; got {:?}",
            resolved
        );
    }
}
