//! 0.9.4 — Linux io_uring kernel-feature probe.
//!
//! Probes which of the elite-tier setup flags
//! (`IORING_SETUP_COOP_TASKRUN`, `IORING_SETUP_SINGLE_ISSUER`,
//! `IORING_SETUP_DEFER_TASKRUN`) the running kernel accepts, then
//! caches the result for the lifetime of the process. Ring
//! constructors elsewhere in the crate consult [`features()`] to
//! decide which flags to set on `io_uring::IoUring::builder()`
//! before calling `.build(queue_depth)`.
//!
//! ## Why probe once, cache forever
//!
//! Each `io_uring_setup(2)` call is a syscall — cheap, but
//! probing every time a ring is constructed wastes work. The
//! kernel cannot change feature support over the process
//! lifetime (a hot kernel upgrade would require a restart), so a
//! single probe at first ring construction is sufficient.
//!
//! ## Probe strategy
//!
//! We try a single ring construction with the most aggressive
//! flag set first. On `EINVAL` we strip the highest-version flag
//! and retry. The walk is:
//!
//! 1. `DEFER_TASKRUN | SINGLE_ISSUER | COOP_TASKRUN` (≥ 6.1)
//! 2. `SINGLE_ISSUER | COOP_TASKRUN`                 (≥ 6.0)
//! 3. `COOP_TASKRUN`                                 (≥ 5.19)
//! 4. (no elite flags)                               (≤ 5.18)
//!
//! `DEFER_TASKRUN` is documented to **require** `SINGLE_ISSUER`,
//! so the two are tested together — there's no useful intermediate.
//!
//! ## What this is not
//!
//! - **Not** a probe for `IORING_SETUP_SQPOLL` or
//!   `IORING_SETUP_IOPOLL`. Both require dedicated cores /
//!   privilege configurations that vary too much per deployment
//!   to enable by default; future patches may add opt-in
//!   `Builder` knobs.
//! - **Not** a probe for `IORING_REGISTER_FILES` /
//!   `IORING_REGISTER_BUFFERS`. Those are register-time, not
//!   setup-time; the ring construction succeeds regardless and
//!   the registration call decides feature support at use time.
//!
//! ## Test surface
//!
//! `cargo test --lib platform::iouring_features` validates the
//! probe runs without panicking and produces a coherent
//! [`IoUringFeatures`] value (every probed flag is independently
//! `bool`-typed; no impossible combinations are produced because
//! `defer_taskrun ⟹ single_issuer` is enforced by the probe).

#![cfg(target_os = "linux")]

use std::sync::OnceLock;

/// Cached snapshot of which io_uring kernel features the host
/// supports. Populated on first call to [`features`]; immutable
/// thereafter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IoUringFeatures {
    /// `IORING_SETUP_COOP_TASKRUN` — cooperative task work
    /// (kernel ≥ 5.19). Reduces inter-processor interrupts on
    /// completion delivery. Safe to enable on any ring; no
    /// runtime conditions.
    pub coop_taskrun: bool,
    /// `IORING_SETUP_SINGLE_ISSUER` — single-issuer hint
    /// (kernel ≥ 6.0). The kernel enforces that all submissions
    /// come from one task; violations fail with `EEXIST`. The
    /// fsys design satisfies this naturally — each ring has a
    /// dedicated owner thread/task, and the submitter is
    /// always the same thread.
    pub single_issuer: bool,
    /// `IORING_SETUP_DEFER_TASKRUN` — defer task work to
    /// submit-time (kernel ≥ 6.1). The application is
    /// responsible for periodically calling
    /// `io_uring_enter(2)` so completions get processed. The
    /// fsys design satisfies this through `submit_and_wait`
    /// (sync path) and the eventfd-driven completion loop
    /// (async path). Requires `single_issuer` (enforced by the
    /// kernel; this struct also enforces it).
    pub defer_taskrun: bool,
}

/// Returns the cached io_uring kernel-feature snapshot, probing
/// on first call.
///
/// The probe runs a single `io_uring_setup(2)` call with the
/// most aggressive flag set the kernel might accept, then strips
/// flags on `EINVAL` and retries. The probe itself opens and
/// immediately closes the ring; no resources are held across
/// calls.
///
/// Returns [`IoUringFeatures::default`] (all-false) on hosts
/// where every probed flag is rejected — the fallback behaviour
/// is identical to pre-0.9.4 (vanilla `IoUring::new`).
pub(crate) fn features() -> IoUringFeatures {
    static CACHE: OnceLock<IoUringFeatures> = OnceLock::new();
    *CACHE.get_or_init(probe)
}

/// Synchronous probe. Tries the most aggressive flag combination
/// first; strips on `EINVAL`. Always returns within microseconds
/// (each `io_uring_setup` is a single syscall).
fn probe() -> IoUringFeatures {
    // Tier 1 — DEFER_TASKRUN (6.1+) requires SINGLE_ISSUER, and
    // pairs naturally with COOP_TASKRUN.
    if try_build(|b| {
        b.setup_defer_taskrun()
            .setup_single_issuer()
            .setup_coop_taskrun();
    }) {
        return IoUringFeatures {
            coop_taskrun: true,
            single_issuer: true,
            defer_taskrun: true,
        };
    }

    // Tier 2 — SINGLE_ISSUER (6.0+) + COOP_TASKRUN.
    if try_build(|b| {
        b.setup_single_issuer().setup_coop_taskrun();
    }) {
        return IoUringFeatures {
            coop_taskrun: true,
            single_issuer: true,
            defer_taskrun: false,
        };
    }

    // Tier 3 — COOP_TASKRUN (5.19+) alone.
    if try_build(|b| {
        b.setup_coop_taskrun();
    }) {
        return IoUringFeatures {
            coop_taskrun: true,
            single_issuer: false,
            defer_taskrun: false,
        };
    }

    // Tier 4 — no elite flags. This is the pre-0.9.4 baseline.
    IoUringFeatures::default()
}

/// Tries building a tiny (queue-depth 4) ring with the flags
/// applied by `cfg`. Returns `true` if construction succeeded,
/// `false` otherwise. The ring is dropped immediately.
fn try_build<F>(cfg: F) -> bool
where
    F: FnOnce(&mut io_uring::Builder),
{
    let mut builder = io_uring::IoUring::builder();
    cfg(&mut builder);
    builder.build(4).is_ok()
}

/// Applies the cached feature set to an `io_uring::Builder`,
/// enabling exactly the flags that the host kernel supports.
///
/// Callers use this from their ring constructors:
///
/// ```text
/// let mut b = io_uring::IoUring::builder();
/// iouring_features::apply(&mut b);
/// let ring = b.build(queue_depth)?;
/// ```
///
/// The builder is mutated in place; the caller retains
/// ownership and may chain additional setup methods after this
/// call. Idempotent — calling `apply` twice is a no-op (each
/// flag is set once at the bit level).
pub(crate) fn apply(builder: &mut io_uring::Builder) {
    let f = features();
    // Always set COOP_TASKRUN first if supported — it's the
    // foundation flag (5.19+) and never requires the others.
    if f.coop_taskrun {
        let _ = builder.setup_coop_taskrun();
    }
    if f.single_issuer {
        let _ = builder.setup_single_issuer();
    }
    // DEFER_TASKRUN requires SINGLE_ISSUER (kernel-enforced).
    // The probe guarantees this co-occurrence; we belt-and-
    // braces gate here as well.
    if f.defer_taskrun && f.single_issuer {
        let _ = builder.setup_defer_taskrun();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe is allowed to return any combination, but it
    /// must respect the kernel's `defer_taskrun ⟹ single_issuer`
    /// constraint — otherwise `apply` would build a ring the
    /// kernel will reject.
    #[test]
    fn defer_taskrun_implies_single_issuer() {
        let f = features();
        if f.defer_taskrun {
            assert!(
                f.single_issuer,
                "DEFER_TASKRUN reported without SINGLE_ISSUER — \
                 the kernel will reject a ring built this way"
            );
        }
    }

    /// `features()` must be a pure cache after the first call —
    /// every subsequent call returns the same value. We can't
    /// observe the cache directly, but we can assert equality
    /// across calls.
    #[test]
    fn features_are_cached_and_stable() {
        let first = features();
        for _ in 0..16 {
            assert_eq!(features(), first);
        }
    }

    /// `apply` must succeed without panicking on any feature
    /// set; the builder is left in a valid state and the
    /// caller can still call `.build`.
    #[test]
    fn apply_does_not_panic_and_builds() {
        let mut b = io_uring::IoUring::builder();
        apply(&mut b);
        // `build(4)` returns `io::Result<IoUring>`. If the
        // probe correctly identified what the kernel accepts,
        // this must succeed. If it doesn't, the probe has
        // overstated capabilities — fail loudly.
        let result = b.build(4);
        assert!(
            result.is_ok(),
            "apply() produced an unbuildable ring on this host: {:?}",
            result.err()
        );
    }
}
