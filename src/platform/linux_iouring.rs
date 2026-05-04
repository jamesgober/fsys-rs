//! Linux `io_uring` wrapper — **stub pending rustc bug fix**.
//!
//! ## Status: blocked on upstream rustc ICE
//!
//! Per locked decision #1 in `.dev/DECISIONS-0.5.0.md`, the elite
//! `Method::Direct` path on Linux is meant to use `io_uring` for
//! submission with a runtime fallback to `pwrite` + `fdatasync` when
//! `io_uring_setup(2)` is unavailable.
//!
//! During checkpoint F+G implementation we hit a reproducible
//! `rustc 1.95.0` internal compiler error (`check_mod_deathness`
//! panic) when this module's implementation methods call into
//! `io_uring::IoUring`'s submit/complete API. The ICE reproduces
//! across:
//!
//! - `io-uring 0.7.x` and `0.6.x`
//! - With and without incremental compilation
//! - With `Mutex<IoUring>`, `RwLock<IoUring>`, and
//!   `UnsafeCell<IoUring>` + companion mutex
//!
//! Since the locked decision specifies a *runtime fallback* on
//! `io_uring_setup` failure, the most expedient correct path is to
//! make this module's `IoUringRing::new` always return
//! [`crate::Error::IoUringSetupFailed`]. Callers (the `Method::Direct`
//! backend in `method/direct.rs`) catch the error and proceed via
//! the existing `O_DIRECT` + `pwrite` + `fdatasync` path that 0.3.0
//! already ships. Functional parity with 0.3.0 is maintained — no
//! performance regression on Linux Direct, just no io_uring uplift.
//!
//! ## Lift path
//!
//! When the upstream rustc bug is fixed (or io-uring publishes a
//! workaround release), restore this module to the version in
//! `.dev/DECISIONS-0.5.0.md`'s R-2''' design appendix. The wrapper
//! shape, synchronization protocol, and SAFETY comments are
//! pre-written there. The stub below need only be replaced; the
//! integration code in `method/direct.rs`, `handle.rs`, and
//! `builder.rs` is designed to work with both the stub and the
//! real wrapper without further changes.

#![cfg(target_os = "linux")]

use crate::{Error, Result};

/// Per-handle io_uring submission ring.
///
/// **0.5.0 stub.** All methods return [`Error::IoUringSetupFailed`]
/// — the runtime fallback path in `Method::Direct` covers this and
/// continues with `pwrite` + `fdatasync` per locked decision #1.
#[allow(dead_code)] // wired into Handle when the upstream rustc bug is fixed
pub(crate) struct IoUringRing;

impl IoUringRing {
    /// **Stub** — always returns [`Error::IoUringSetupFailed`].
    #[allow(dead_code)] // wired into Handle when the upstream rustc bug is fixed
    pub(crate) fn new(_queue_depth: u32) -> Result<Self> {
        Err(Error::IoUringSetupFailed {
            source: std::io::Error::other(
                "io_uring wrapper stubbed in 0.5.0 pending upstream rustc fix",
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_returns_setup_failed_in_0_5_0_stub() {
        match IoUringRing::new(8) {
            Err(Error::IoUringSetupFailed { source: _ }) => {}
            Ok(_) => panic!("0.5.0 stub must always return IoUringSetupFailed"),
            Err(other) => panic!("unexpected variant: {:?}", other),
        }
    }
}
