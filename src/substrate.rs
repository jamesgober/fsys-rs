//! [`AsyncSubstrate`] — which runtime substrate the async layer uses.
//!
//! The async layer (gated behind the `async` Cargo feature) has two
//! execution paths since `0.7.0`:
//!
//! 1. **Native io_uring substrate** — Linux only, when
//!    [`Method::Direct`](crate::Method::Direct) is in use, the
//!    per-handle io_uring ring is constructed, and the
//!    `FSYS_DISABLE_NATIVE_ASYNC=1` environment override is **not**
//!    set. Async ops submit directly to the ring and `.await` a
//!    `oneshot` driven by a per-handle completion driver task; no
//!    `spawn_blocking` thread-pool hop.
//! 2. **`spawn_blocking` fallback** — every other configuration
//!    (Windows, macOS, sync method, io_uring unavailable, override
//!    set). Mirrors the 0.6.0 baseline.
//!
//! The enum + accessor are always present. On non-Linux builds and
//! on Linux without the `async` feature, [`Handle::async_substrate`](crate::Handle::async_substrate)
//! returns [`AsyncSubstrate::SpawnBlocking`] (the substrate that
//! *would* be used if the caller enabled the feature) so consumers
//! can always read the value without `cfg`-gating their match arms.
//!
//! ## Asymmetry by design
//!
//! Per locked decision `D-1` in `.dev/DECISIONS-0.7.0.md`, the
//! native substrate is Linux-only. Windows and macOS async stay on
//! `spawn_blocking` indefinitely — there's no clean primitive for a
//! native async fast path on those platforms (Windows IOCP would
//! work but isn't justified by demand; macOS has no equivalent).
//! The asymmetry is documented honestly rather than papered over.

/// Which runtime substrate the async layer uses for a given handle.
///
/// Read at runtime via [`crate::Handle::async_substrate`]. The value
/// is computed on each call (it's cheap — three atomic loads and a
/// pointer check) so callers always see the current truth: e.g.
/// when the io_uring ring is lazily constructed on the first Direct
/// op, the substrate transitions from `SpawnBlocking` (pre-probe)
/// to `NativeIoUring` (post-probe) without further configuration.
///
/// # Examples
///
/// ```no_run
/// # async fn example() -> fsys::Result<()> {
/// let fs = std::sync::Arc::new(fsys::builder().build()?);
/// match fs.async_substrate() {
///     fsys::AsyncSubstrate::NativeIoUring => {
///         // Linux + Method::Direct + io_uring active.
///         // Async ops submit directly to the ring.
///     }
///     fsys::AsyncSubstrate::SpawnBlocking => {
///         // Cross-platform fallback — async ops use
///         // tokio::task::spawn_blocking.
///     }
///     // `AsyncSubstrate` is `#[non_exhaustive]` for forward
///     // compatibility (e.g. a future Windows IOCP substrate).
///     _ => unreachable!(),
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AsyncSubstrate {
    /// Native io_uring submission path. Available only on Linux,
    /// only when `Method::Direct` is in use and the per-handle
    /// io_uring ring is constructed and live, and only when
    /// `FSYS_DISABLE_NATIVE_ASYNC` is not set.
    NativeIoUring,
    /// `tokio::task::spawn_blocking` fallback. Cross-platform; the
    /// only substrate available on Windows and macOS, and the
    /// fallback on Linux when conditions for the native substrate
    /// aren't met.
    SpawnBlocking,
}

impl AsyncSubstrate {
    /// Returns `true` if this is the native io_uring substrate.
    #[must_use]
    pub fn is_native(self) -> bool {
        matches!(self, AsyncSubstrate::NativeIoUring)
    }

    /// Returns `true` if this is the `spawn_blocking` fallback.
    #[must_use]
    pub fn is_blocking(self) -> bool {
        matches!(self, AsyncSubstrate::SpawnBlocking)
    }

    /// A static string describing this substrate for display.
    /// Stable across `0.x.y`; do not rely on the exact wording for
    /// programmatic logic.
    ///
    /// Note: this is an *accessor* (the substrate's
    /// human-readable name), not a `String`-conversion helper.
    /// Compare to `as_str` on `String` / `PathBuf` which exists
    /// to expose underlying string storage.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            AsyncSubstrate::NativeIoUring => "native io_uring",
            AsyncSubstrate::SpawnBlocking => "spawn_blocking",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_native_true_only_for_native() {
        assert!(AsyncSubstrate::NativeIoUring.is_native());
        assert!(!AsyncSubstrate::SpawnBlocking.is_native());
    }

    #[test]
    fn is_blocking_true_only_for_blocking() {
        assert!(AsyncSubstrate::SpawnBlocking.is_blocking());
        assert!(!AsyncSubstrate::NativeIoUring.is_blocking());
    }

    #[test]
    fn name_matches_enum() {
        assert_eq!(AsyncSubstrate::NativeIoUring.name(), "native io_uring");
        assert_eq!(AsyncSubstrate::SpawnBlocking.name(), "spawn_blocking");
    }

    #[test]
    fn is_copy_and_eq() {
        let a = AsyncSubstrate::NativeIoUring;
        let b = a;
        assert_eq!(a, b);
    }
}
