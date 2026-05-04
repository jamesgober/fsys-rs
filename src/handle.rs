//! The [`Handle`] struct — the primary entry point for file IO operations.
//!
//! A `Handle` captures the resolved configuration (method, root directory,
//! mode, probed sector size) and provides all CRUD operations through its
//! `impl` blocks defined in [`crate::crud`].
//!
//! `Handle` is `Send + Sync`: the mutable state (active method) is managed
//! with atomic operations.

use crate::method::Method;
use crate::path::Mode;
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicU8, Ordering};

// ──────────────────────────────────────────────────────────────────────────────
// Write-counter for unique temp-file names
// ──────────────────────────────────────────────────────────────────────────────

/// Process-global monotonic counter for generating unique temp-file names.
///
/// Using a global counter (rather than per-handle) ensures uniqueness even
/// when multiple handles share the same root directory.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

// ──────────────────────────────────────────────────────────────────────────────

/// The primary entry point for all fsys file IO operations.
///
/// A `Handle` holds the resolved configuration for a single IO context:
/// durability method, root directory scope, operating mode, and probed
/// sector size. All CRUD methods are implemented as `impl Handle` blocks in
/// the [`crate::crud`] module.
///
/// # Thread safety
///
/// `Handle` is `Send + Sync`. The [`active_method`](Handle::active_method)
/// field is managed with atomic operations so multiple threads can share a
/// single `Handle` without additional locking.
///
/// # Building a Handle
///
/// Use [`crate::builder()`] (preferred) or [`crate::new()`] for a
/// zero-configuration default:
///
/// ```
/// # fn example() -> fsys::Result<()> {
/// let handle = fsys::builder()
///     .method(fsys::Method::Auto)
///     .build()?;
/// # Ok(())
/// # }
/// ```
pub struct Handle {
    /// The method explicitly requested by the caller (possibly `Auto`).
    configured_method: AtomicU8,
    /// The method currently in effect after runtime fallbacks.
    ///
    /// Set to the resolved form of `configured_method` at build time.
    /// May be updated to a less-capable method if the OS rejects a
    /// privileged open (e.g. `O_DIRECT` rejected on tmpfs → falls back
    /// to `Data`).
    active_method: AtomicU8,
    /// Optional root directory. When set, all relative paths are resolved
    /// against this root and path-escape checks are enforced.
    root: Option<PathBuf>,
    /// Operating mode — affects default path selection.
    mode: Mode,
    /// Probed logical sector size for aligned Direct IO buffers (bytes).
    sector_size: u32,
}

impl Handle {
    /// Creates a `Handle` from raw components.
    ///
    /// This is `pub(crate)` — external callers use [`crate::Builder`].
    pub(crate) fn new_raw(
        configured_method: Method,
        active_method: Method,
        root: Option<PathBuf>,
        mode: Mode,
        sector_size: u32,
    ) -> Self {
        Self {
            configured_method: AtomicU8::new(configured_method.to_u8()),
            active_method: AtomicU8::new(active_method.to_u8()),
            root,
            mode,
            sector_size,
        }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Public accessors
    // ──────────────────────────────────────────────────────────────────────────

    /// Returns the method that was configured by the caller.
    ///
    /// This may be [`Method::Auto`] if the caller did not specify a method;
    /// see [`Handle::active_method`] for the resolved value.
    #[must_use]
    pub fn method(&self) -> Method {
        Method::from_u8(self.configured_method.load(Ordering::Relaxed))
    }

    /// Returns the method currently in effect after any runtime fallbacks.
    ///
    /// This is always a concrete method (`Sync`, `Data`, or `Direct`) —
    /// never `Auto`. If `O_DIRECT` was rejected at open time and the
    /// handle fell back to `Data`, this method will reflect that change.
    #[must_use]
    pub fn active_method(&self) -> Method {
        Method::from_u8(self.active_method.load(Ordering::Relaxed))
    }

    /// Updates the configured method for future IO operations.
    ///
    /// Returns [`Error::UnsupportedMethod`] for reserved variants
    /// ([`Method::Mmap`] and [`Method::Journal`]).
    pub fn set_method(&self, method: Method) -> Result<()> {
        if method.is_reserved() {
            return Err(Error::UnsupportedMethod {
                method: method.as_str(),
            });
        }
        let resolved = method.resolve();
        self.configured_method
            .store(method.to_u8(), Ordering::Relaxed);
        self.active_method
            .store(resolved.to_u8(), Ordering::Relaxed);
        Ok(())
    }

    /// Returns the root directory scope, if one was configured.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Returns the operating mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Returns the probed logical sector size in bytes.
    ///
    /// Used to size aligned Direct IO buffers.
    #[must_use]
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Crate-internal helpers
    // ──────────────────────────────────────────────────────────────────────────

    /// Updates the active method after a runtime fallback.
    ///
    /// Called by IO functions when the OS rejects a privileged flag (e.g.
    /// `O_DIRECT` on tmpfs). Takes effect for all subsequent operations on
    /// this handle.
    pub(crate) fn update_active_method(&self, method: Method) {
        self.active_method.store(method.to_u8(), Ordering::Relaxed);
    }

    /// Returns `true` if the active method requires Direct IO.
    pub(crate) fn use_direct(&self) -> bool {
        self.active_method() == Method::Direct
    }

    /// Resolves a caller-supplied path against this handle's root.
    ///
    /// If the handle has a root:
    /// - Absolute paths are checked to ensure they are rooted *inside* the
    ///   handle root (rejects path-escape attacks).
    /// - Relative paths are joined to the root.
    ///
    /// If the handle has no root, the path is returned as-is.
    pub(crate) fn resolve_path(&self, path: &Path) -> Result<PathBuf> {
        let Some(root) = &self.root else {
            return Ok(path.to_owned());
        };

        let candidate = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };

        // Canonicalise components without touching the filesystem so that
        // a path like `root/a/../../../etc/passwd` is caught before any
        // syscall. We do a simple lexical normalisation: process each
        // component and reject `..` that would escape the root.
        let mut resolved = PathBuf::new();
        for component in candidate.components() {
            use std::path::Component;
            match component {
                Component::Prefix(p) => {
                    resolved.push(p.as_os_str());
                }
                Component::RootDir => {
                    resolved.push(component);
                }
                Component::CurDir => {
                    // Skip `.`
                }
                Component::ParentDir => {
                    if !resolved.pop() {
                        return Err(Error::InvalidPath {
                            path: path.to_owned(),
                            reason: "path escapes the handle root".into(),
                        });
                    }
                }
                Component::Normal(n) => {
                    resolved.push(n);
                }
            }
        }

        // Final check: the resolved path must start with the root.
        if !resolved.starts_with(root) {
            return Err(Error::InvalidPath {
                path: path.to_owned(),
                reason: "path escapes the handle root".into(),
            });
        }

        Ok(resolved)
    }

    /// Generates a unique temp-file path adjacent to `path`.
    ///
    /// The temp name is `.fsys-tmp-<counter>.<filename>` so it sorts near
    /// the target and is identifiable in crash recovery. If the target has
    /// no file name the counter alone is used.
    pub(crate) fn gen_temp_path(path: &Path) -> PathBuf {
        let n = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let stem = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = format!(".fsys-tmp-{}.{}", n, stem);
        parent.join(name)
    }
}

// Handle is Send + Sync because AtomicU8 and AtomicU64 are Send + Sync,
// Option<PathBuf> is Send + Sync, Mode is Copy, and u32 is Copy.
// The compiler will derive these automatically, but asserting them here
// makes any future regression a compile error rather than a runtime surprise.
const _: () = {
    #[allow(dead_code)]
    fn assert_send<T: Send>() {}
    #[allow(dead_code)]
    fn assert_sync<T: Sync>() {}
    #[allow(dead_code)]
    fn check() {
        assert_send::<Handle>();
        assert_sync::<Handle>();
    }
};

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use crate::path::Mode;

    fn make_handle(method: Method) -> Handle {
        Handle::new_raw(method, method.resolve(), None, Mode::Dev, 512)
    }

    #[test]
    fn test_method_accessor_roundtrip() {
        let h = make_handle(Method::Sync);
        assert_eq!(h.method(), Method::Sync);
    }

    #[test]
    fn test_active_method_reflects_resolved() {
        let h = make_handle(Method::Auto);
        let active = h.active_method();
        assert_ne!(active, Method::Auto, "active method must be concrete");
    }

    #[test]
    fn test_set_method_updates_active() {
        let h = make_handle(Method::Sync);
        h.set_method(Method::Data).expect("set_method");
        assert_eq!(h.method(), Method::Data);
    }

    #[test]
    fn test_set_reserved_method_returns_error() {
        let h = make_handle(Method::Sync);
        let err = h.set_method(Method::Mmap);
        assert!(err.is_err());
        if let Err(Error::UnsupportedMethod { method }) = err {
            assert_eq!(method, "mmap");
        } else {
            panic!("expected UnsupportedMethod");
        }
    }

    #[test]
    fn test_use_direct_reflects_method() {
        let h = Handle::new_raw(Method::Direct, Method::Direct, None, Mode::Dev, 512);
        assert!(h.use_direct());
        let h2 = make_handle(Method::Sync);
        assert!(!h2.use_direct());
    }

    #[test]
    fn test_resolve_path_no_root_passthrough() {
        let h = make_handle(Method::Sync);
        let p = PathBuf::from("some/relative/path");
        assert_eq!(h.resolve_path(&p).expect("resolve"), p);
    }

    #[test]
    fn test_resolve_path_with_root_joins() {
        let root = std::env::temp_dir();
        let h = Handle::new_raw(
            Method::Sync,
            Method::Sync,
            Some(root.clone()),
            Mode::Dev,
            512,
        );
        let resolved = h
            .resolve_path(Path::new("subdir/file.txt"))
            .expect("resolve");
        assert!(resolved.starts_with(&root));
    }

    #[test]
    fn test_resolve_path_escape_is_rejected() {
        let root = std::env::temp_dir().join("jail");
        let h = Handle::new_raw(Method::Sync, Method::Sync, Some(root), Mode::Dev, 512);
        let result = h.resolve_path(Path::new("../../etc/passwd"));
        assert!(result.is_err(), "path escape must be rejected");
    }

    #[test]
    fn test_gen_temp_path_has_fsys_prefix() {
        let path = PathBuf::from("/tmp/myfile.db");
        let tmp = Handle::gen_temp_path(&path);
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".fsys-tmp-"), "got: {}", name);
    }

    #[test]
    fn test_sector_size_accessor() {
        let h = Handle::new_raw(Method::Sync, Method::Sync, None, Mode::Dev, 4096);
        assert_eq!(h.sector_size(), 4096);
    }
}
