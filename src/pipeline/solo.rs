//! Solo lane — single-write, low-latency dispatch path.
//!
//! There is no actual subsystem here. The "solo lane" is a label for
//! the routing decision: single-op calls (`Handle::write`, `read`,
//! `append`, `write_at`, `delete`, `copy`, `meta`, `sync`, etc.) bypass
//! the pipeline entirely and call directly into [`crate::crud::file`]
//! and [`crate::crud::dir`] with **zero queueing or dispatch overhead**.
//!
//! The implementation lives where it belongs: alongside the rest of the
//! solo CRUD code in `crud/file.rs` and `crud/dir.rs`. This file exists
//! only to:
//!
//! 1. Keep the prompt-mandated `pipeline/solo.rs` file present so the
//!    module tree matches `.dev/PLANNING.md`.
//! 2. Provide a single anchor point for documenting the routing
//!    invariant — see decision R-11 in `.dev/DECISIONS-0.4.0.md`.
//! 3. Reserve a place for a future instrumentation hook (e.g.
//!    `tracing` spans on solo-lane latency in the `0.7.0`
//!    observability pass).
//!
//! ## Invariant
//!
//! **No code on the solo path may touch [`super::Pipeline`].** A solo
//! op is byte-for-byte identical between `0.3.0` and `0.4.0`. This is
//! the locked decision #2 from the prompt: routing is explicit, never
//! automatic.

/// Marker type for the solo lane.
///
/// Has no fields and no behaviour. Its only purpose is to give the solo
/// lane a typeable identity for documentation cross-references and
/// future instrumentation hooks. **Never construct one directly** —
/// solo-lane ops do not flow through any object; they are direct
/// function calls on [`crate::Handle`].
#[allow(dead_code)] // intentional: this is a documentation anchor
pub(crate) struct SoloLane;
