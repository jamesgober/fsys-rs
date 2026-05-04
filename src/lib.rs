//! # fsys
//!
//! Adaptive file and directory IO for Rust — fast, hardware-aware, multi-strategy.
//!
//! `fsys` is a low-level filesystem abstraction designed for storage engines,
//! databases, and any application that needs predictable, high-performance
//! file IO with explicit control over durability strategy.
//!
//! ## What ships in `0.4.0`
//!
//! Adds the dual-pipeline model on top of the `0.3.0` foundation:
//!
//! - [`Handle::write_batch`], [`Handle::delete_batch`],
//!   [`Handle::copy_batch`], and [`Handle::batch`]: the group-lane
//!   batch API. Routes through a per-handle dispatcher thread that is
//!   spawned lazily on first use and shut down cleanly on `Handle`
//!   drop. Hybrid time-or-count window (default 1 ms / 128 ops),
//!   bounded queue (default 1024) with blocking submission.
//! - [`Batch`]: a chainable builder for very large or dynamic batches.
//! - [`BatchError`]: per-batch failure reporting with `failed_at` /
//!   `completed` / `source`.
//! - [`Error::ShutdownInProgress`] (FS-00009) and reserved
//!   [`Error::QueueFull`] (FS-00010, never emitted in 0.4.0).
//!
//! Solo-lane ops (`write`, `read`, `append`, etc.) are byte-for-byte
//! identical to `0.3.0` — the pipeline is invisible on that path.
//!
//! ## What shipped in `0.3.0`
//!
//! - [`Error`] enum and [`Result`] type alias.
//! - [`Handle`]: the primary IO entry point — holds method, root, sector size.
//! - [`Builder`]: fluent builder for constructing a `Handle`.
//! - [`Method`]: durability strategy enum — `Sync`, `Data`, `Direct`, `Auto`.
//! - [`FileMeta`], [`DirEntry`], [`Permissions`]: filesystem metadata types.
//! - [`crud`]: file and directory CRUD as `impl Handle`.
//! - [`quick`]: convenience free functions backed by a default `Handle`.
//! - [`os`] module: OS detection.
//! - [`hardware`] module: hardware probe stubs.
//! - [`path`] module: per-OS path defaults and `Mode`.
//!
//! ## Quick start
//!
//! ```no_run
//! # fn example() -> fsys::Result<()> {
//! // One-shot write/read (uses a lazily-initialised default Handle):
//! fsys::quick::write("/tmp/hello.txt", b"hello")?;
//! let data = fsys::quick::read("/tmp/hello.txt")?;
//!
//! // Explicit Handle with a specific method:
//! let h = fsys::builder().method(fsys::Method::Data).build()?;
//! h.write("/tmp/world.txt", b"world")?;
//! # Ok(())
//! # }
//! ```
//!
//! ### Batch operations (0.4.0)
//!
//! Multiple ops share a single dispatcher round-trip and one durability
//! pass per modified file. The dispatcher thread is spawned lazily on
//! the first batch op and shut down cleanly on `Handle` drop.
//!
//! ```no_run
//! # fn example() {
//! let h = fsys::new().expect("handle");
//!
//! // Slice-based batch — best when ops are already collected.
//! h.write_batch(&[
//!     ("/tmp/a.bin", b"alpha".as_slice()),
//!     ("/tmp/b.bin", b"beta".as_slice()),
//! ])
//! .expect("batch");
//!
//! // Builder-based batch — best for large or dynamic batches.
//! let mut batch = h.batch();
//! let _ = batch.write("/tmp/c.bin", b"gamma");
//! let _ = batch.delete("/tmp/stale.tmp");
//! batch.commit().expect("commit");
//! # }
//! ```
//!
//! ## Goals
//!
//! - **Hardware-aware.** Detect drive type (NVMe, SSD, HDD) and capabilities
//!   at startup. Pick sensible defaults.
//! - **Multi-strategy durability.** Support `fsync`, `fdatasync`, Direct IO.
//! - **Cross-platform.** Linux, macOS, Windows. Best path on each.
//! - **Zero magic.** Every strategy is explicit. No hidden buffering.

#![doc(html_root_url = "https://docs.rs/fsys/0.4.0")]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::unreachable)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(clippy::missing_safety_doc)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]
#![warn(rust_2018_idioms)]
#![warn(clippy::all)]

pub mod batch;
pub(crate) mod buffer;
pub mod builder;
pub mod crud;
pub mod error;
pub mod handle;
pub mod hardware;
pub mod meta;
pub mod method;
pub mod os;
pub mod path;
pub(crate) mod pipeline;
pub(crate) mod platform;
pub mod quick;

pub use crate::batch::Batch;
pub use crate::builder::Builder;
pub use crate::error::{BatchError, Error, Result};
pub use crate::handle::Handle;
pub use crate::meta::{DirEntry, FileMeta, Permissions};
pub use crate::method::Method;
pub use crate::path::Mode;

/// Creates a default [`Handle`] using [`Method::Auto`] and no root scope.
///
/// Equivalent to `Builder::new().build()`.
///
/// # Errors
///
/// Returns an error if method validation fails (this will not happen for
/// the default [`Method::Auto`] setting).
pub fn new() -> Result<Handle> {
    Builder::new().build()
}

/// Creates a [`Handle`] using the specified [`Method`].
///
/// # Errors
///
/// Returns [`Error::UnsupportedMethod`] if a reserved method variant is
/// supplied.
pub fn with(method: Method) -> Result<Handle> {
    Builder::new().method(method).build()
}

/// Returns a new [`Builder`] with default settings.
///
/// # Example
///
/// ```
/// # fn example() -> fsys::Result<()> {
/// let handle = fsys::builder().method(fsys::Method::Sync).build()?;
/// # Ok(())
/// # }
/// ```
#[must_use]
pub fn builder() -> Builder {
    Builder::new()
}

/// Library version, matching the crate version declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn version_matches_cargo() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }
}
