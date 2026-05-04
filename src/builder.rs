//! [`Builder`] for constructing a configured [`Handle`].
//!
//! # Example
//!
//! ```
//! # fn example() -> fsys::Result<()> {
//! use fsys::{Builder, Method, Mode};
//!
//! let handle = Builder::new()
//!     .method(Method::Data)
//!     .mode(Mode::Dev)
//!     .build()?;
//! # Ok(())
//! # }
//! ```

use crate::handle::Handle;
use crate::method::Method;
use crate::path::Mode;
use crate::{Error, Result};
use std::path::PathBuf;

/// A builder for creating a [`Handle`].
///
/// Obtain one via [`crate::builder()`] or [`Builder::new()`].
///
/// All fields are optional. Unset fields use sensible defaults:
/// - `method` defaults to [`Method::Auto`] (hardware-aware selection).
/// - `root` defaults to `None` (no path scope enforcement).
/// - `mode` defaults to [`Mode::Auto`] (resolved from environment).
pub struct Builder {
    method: Method,
    root: Option<PathBuf>,
    mode: Mode,
}

impl Builder {
    /// Creates a new `Builder` with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            method: Method::Auto,
            root: None,
            mode: Mode::Auto,
        }
    }

    /// Sets the durability method.
    ///
    /// Returns an error at [`build`](Builder::build) time if a reserved
    /// variant ([`Method::Mmap`] or [`Method::Journal`]) is supplied.
    #[must_use]
    pub fn method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    /// Restricts all IO to paths under `root`.
    ///
    /// When set, handle path resolution enforces that every path stays
    /// within this root. Relative paths are joined to the root; absolute
    /// paths that escape the root are rejected with
    /// [`Error::InvalidPath`].
    #[must_use]
    pub fn root<P: Into<PathBuf>>(mut self, root: P) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Sets the operating mode.
    ///
    /// Affects default path selection; [`Mode::Auto`] resolves from the
    /// `FSYS_MODE` / `RUST_ENV` environment variables.
    #[must_use]
    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    /// Constructs the [`Handle`].
    ///
    /// Resolves `Method::Auto` using the hardware-detection ladder,
    /// probes the sector size for the root (or current directory), and
    /// validates that no reserved method was requested.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedMethod`] if a reserved method variant
    /// was supplied.
    pub fn build(self) -> Result<Handle> {
        if self.method.is_reserved() {
            return Err(Error::UnsupportedMethod {
                method: self.method.as_str(),
            });
        }

        let resolved_method = self.method.resolve();
        let mode = self.mode.resolve();

        // Probe sector size for the target directory (or cwd as fallback).
        let probe_path = self
            .root
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."));
        let sector_size = crate::platform::probe_sector_size(probe_path);

        Ok(Handle::new_raw(
            self.method,
            resolved_method,
            self.root,
            mode,
            sector_size,
        ))
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;

    #[test]
    fn test_default_build_succeeds() {
        let h = Builder::new().build().expect("default build");
        // active_method must be concrete (not Auto)
        assert_ne!(h.active_method(), Method::Auto);
    }

    #[test]
    fn test_builder_sets_method() {
        let h = Builder::new()
            .method(Method::Sync)
            .build()
            .expect("build with Sync");
        assert_eq!(h.method(), Method::Sync);
        assert_eq!(h.active_method(), Method::Sync);
    }

    #[test]
    fn test_builder_sets_root() {
        let root = std::env::temp_dir();
        let h = Builder::new()
            .root(root.clone())
            .build()
            .expect("build with root");
        assert_eq!(h.root(), Some(root.as_path()));
    }

    #[test]
    fn test_builder_rejects_reserved_method() {
        let err = Builder::new().method(Method::Mmap).build();
        assert!(err.is_err());
        if let Err(Error::UnsupportedMethod { method }) = err {
            assert_eq!(method, "mmap");
        } else {
            panic!("expected UnsupportedMethod");
        }
    }

    #[test]
    fn test_builder_sector_size_at_least_512() {
        let h = Builder::new().build().expect("build");
        assert!(h.sector_size() >= 512);
    }
}
