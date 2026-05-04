//! Explicit routing logic — solo vs group lane.
//!
//! Per the locked decision #2 in the prompt and decision R-12 in
//! `.dev/DECISIONS-0.4.0.md`, routing is **explicit and never
//! automatic**. The mapping is fixed:
//!
//! | Surface call | Lane |
//! |---|---|
//! | `Handle::write`, `read`, `append`, `write_at` | solo (direct CRUD call) |
//! | `Handle::delete`, `copy`, `exists`, `size`, `meta`, `sync` | solo (direct CRUD call) |
//! | `Handle::mkdir`, `mkdir_all`, `rmdir`, `rmdir_all`, `list`, `is_dir`, `is_file` | solo |
//! | `Handle::write_batch`, `delete_batch`, `copy_batch` | group |
//! | `Batch::commit` (from `batch.rs`) | group |
//!
//! There is no automatic detection, no opportunistic batching, no
//! threshold-based promotion of single ops to batches. A given call site
//! has predictable, documented latency characteristics.
//!
//! ## Why this module is so small
//!
//! In `0.4.0`, every batch op routes to the group lane unconditionally,
//! and every non-batch op routes to the solo lane unconditionally. The
//! routing function below is a sentinel, not a switch — it simply
//! returns [`Lane::Group`] for the calls that consult it. The function
//! exists so future phases (a Cargo feature for opt-in inline-batching,
//! an environment-variable override for testing, etc.) have a single
//! place to grow without disturbing the public API.

/// Lanes the dispatcher can route work to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // SoloLane is never returned in 0.4.0; reserved for future use
pub(crate) enum Lane {
    /// Direct CRUD call, no queueing. See [`super::solo`].
    Solo,
    /// Bounded queue + per-handle dispatcher thread. See
    /// [`super::group`].
    Group,
}

/// Returns the lane that batch ops route to. Always [`Lane::Group`] in
/// `0.4.0`.
///
/// Existence justification: see this module's doc comment.
#[inline]
#[allow(dead_code)] // surfaced via Handle integration in checkpoint C
pub(crate) const fn batch_lane() -> Lane {
    Lane::Group
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_lane_is_group_in_0_4_0() {
        assert_eq!(batch_lane(), Lane::Group);
    }
}
