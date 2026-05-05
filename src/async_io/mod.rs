//! Async wrappers for fsys's sync IO core.
//!
//! Gated behind the `async` Cargo feature. When enabled, every
//! sync method on [`crate::Handle`] gets an `_async` sibling whose
//! implementation is [`tokio::task::spawn_blocking`] calling the
//! existing sync method on tokio's blocking pool. Async batch
//! operations route through the same per-handle dispatcher as sync
//! batches, but the response channel is
//! [`tokio::sync::oneshot`] so the caller `.await`s without
//! blocking a thread-pool slot.
//!
//! ## Why `spawn_blocking`?
//!
//! Locked decision D-1 in `.dev/DECISIONS-0.6.0.md`:
//! `spawn_blocking` is the canonical async/sync bridge in the Rust
//! async ecosystem — well-tested, plays correctly with tokio
//! backpressure, and adds zero new threads beyond tokio's existing
//! blocking pool. Native io_uring async (using io_uring as a true
//! async substrate, with `Future`-driven completion) is locked for
//! `0.7.0`.
//!
//! ## Sync interop
//!
//! Calling sync `fs.write()` from inside a tokio runtime is
//! supported and well-defined — it blocks the calling thread until
//! the IO completes. To avoid blocking the runtime's worker, callers
//! who care about async correctness should use the `_async` sibling.
//!
//! Calling async `fs.write_async()` outside a tokio runtime returns
//! [`crate::Error::AsyncRuntimeRequired`]. This is graceful (no
//! panic) — the call site observes a regular error and can react.
//!
//! ## Buffer ownership
//!
//! `spawn_blocking` requires `'static` payloads. Async methods take
//! owned buffers (`Vec<u8>` instead of `&[u8]`) and resolve owned
//! paths (`PathBuf` instead of `&Path`). The cost is one
//! `to_owned` / `to_vec` per call — negligible compared to the IO
//! itself.

pub mod batch;
pub mod crud_dir;
pub mod crud_file;
pub mod quick;

/// Returns an [`crate::Error::AsyncRuntimeRequired`] error when the
/// caller invokes an `_async` method outside a tokio runtime.
///
/// Used internally by the `_async` wrappers; surfaced publicly so
/// callers can construct the same error in their own code if they
/// build adapters on top of fsys's async surface.
pub(crate) fn require_runtime() -> Result<(), crate::Error> {
    if tokio::runtime::Handle::try_current().is_ok() {
        Ok(())
    } else {
        Err(crate::Error::AsyncRuntimeRequired)
    }
}
