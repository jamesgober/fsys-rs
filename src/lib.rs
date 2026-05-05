//! # fsys
//!
//! Adaptive file and directory IO for Rust — fast, hardware-aware,
//! multi-strategy.
//!
//! `fsys` is a low-level filesystem abstraction designed for storage
//! engines, databases, and any application that needs predictable,
//! high-performance file IO with explicit control over durability
//! strategy.
//!
//! ## Three tiers of API
//!
//! ### Tier 1 — one-shot helpers
//!
//! The simplest path. Uses a lazily-initialised default [`Handle`]
//! configured with [`Method::Auto`].
//!
//! ```no_run
//! # fn example() -> fsys::Result<()> {
//! fsys::quick::write("/tmp/greeting.txt", b"hello")?;
//! let data = fsys::quick::read("/tmp/greeting.txt")?;
//! assert_eq!(data, b"hello");
//! # Ok(())
//! # }
//! ```
//!
//! ### Tier 2 — handle-based
//!
//! The primary API for everything beyond one-shot use. Construct a
//! [`Handle`] with [`new()`] (default `Method::Auto`) or
//! [`with(method)`](with).
//!
//! ```no_run
//! # fn example() -> fsys::Result<()> {
//! let fs = fsys::new()?;                          // Method::Auto
//! let fs = fsys::with(fsys::Method::Data)?;       // explicit method
//! fs.write("/tmp/world.txt", b"world")?;
//! let read = fs.read("/tmp/world.txt")?;
//! # Ok(())
//! # }
//! ```
//!
//! ### Tier 3 — full builder
//!
//! For advanced configuration: custom root, dev/prod mode,
//! per-handle batch knobs, io_uring queue depth, buffer pool size.
//!
//! ```no_run
//! # fn example() -> fsys::Result<()> {
//! let fs = fsys::builder()
//!     .method(fsys::Method::Direct)
//!     .root("/var/lib/myapp")
//!     .mode(fsys::Mode::Prod)
//!     .build()?;
//! # Ok(())
//! # }
//! ```
//!
//! ## What's in 0.9.0 (release candidate for 1.0)
//!
//! 0.9.0 is the **release candidate for 1.0**. Real-world testing
//! begins from this tag. The public API documented in
//! [`docs/API.md`](https://github.com/jamesgober/fsys-rs/blob/main/docs/API.md)
//! is the 1.0 target shape.
//!
//! - **Journal substrate** — open-once append-only log for WAL-style
//!   workloads. [`JournalHandle::append`](crate::JournalHandle::append)
//!   is the high-throughput durable-write primitive that databases /
//!   queues / ledgers should use; [`Handle::write`] is the
//!   atomic-replace primitive for individual files. Three throughput
//!   tiers ship: cross-platform sync, lock-free concurrent append,
//!   and native io_uring async on Linux.
//! - **Direct-IO journal opt-in** —
//!   [`JournalOptions::direct(true)`](crate::JournalOptions::direct)
//!   routes appends through a sector-aligned in-memory log buffer
//!   (the InnoDB / WiredTiger pattern), bypassing the kernel page
//!   cache for zero-copy DMA on NVMe.
//! - **Production-grade frame format** — every record wrapped in
//!   a 12-byte frame with CRC-32C (Castagnoli, RFC 3720 KAT-verified).
//!   Tail-truncation detection via [`JournalTailState`].
//! - **Crash-safety integration tests** — process-kill harness
//!   validates durability claims under real crashes.
//! - **Optional `tracing` feature** for production observability.
//!
//! ## What shipped in 0.6.0–0.7.0
//!
//! 0.6.0 finished the public API and 0.7.0 (the optimization
//! phase) tuned it. Every method that will ship at 1.0 is
//! present from 0.7.0 onward.
//!
//! - **Async layer** (gated behind the `async` Cargo feature). Every
//!   sync method gets an `_async` sibling backed by
//!   `tokio::task::spawn_blocking`; async batch ops route through the
//!   per-handle dispatcher via `tokio::sync::oneshot`.
//! - **NVMe passthrough flush** on Linux (`NVME_IOCTL_IO_CMD`) and
//!   Windows (`IOCTL_STORAGE_PROTOCOL_COMMAND`). Capability detection
//!   at first Direct op; transparent fallback to `fdatasync` /
//!   `WRITE_THROUGH` on incapable hardware. macOS uses
//!   `F_NOCACHE + F_FULLFSYNC` (Apple does not expose NVMe
//!   passthrough).
//! - **Completion CRUD:** [`Handle::write_copy`] (atomic-swap with
//!   metadata preservation), [`Handle::scan`], [`Handle::find`]
//!   (glob), [`Handle::count`], [`Handle::truncate`],
//!   [`Handle::rename`].
//! - **`Handle::active_durability_primitive()`** + [`mod@primitive`]
//!   constants — the canonical name of the durability primitive
//!   currently in effect.
//!
//! ## What shipped earlier
//!
//! - **0.5.x:** real hardware probe, `Method::Mmap`, `Method::Direct`
//!   with io_uring on Linux, per-method crash tests, per-handle
//!   aligned buffer pool. 0.5.1 unstubbed the real io_uring path.
//! - **0.4.0:** dual-pipeline model. Solo lane (single writes via
//!   the calling thread) + group lane (batch ops via a per-handle
//!   dispatcher).
//! - **0.3.0:** [`Handle`], [`Builder`], full file/dir CRUD,
//!   cross-platform Direct IO with observable fallback.
//! - **0.2.0:** [`Error`] / [`Result`], hardware probe stubs, OS
//!   detection, path resolution.
//!
//! ## Choosing a method
//!
//! | If you... | Pick |
//! |---|---|
//! | Don't know what you need | [`Method::Auto`] |
//! | Need universal correctness floor | [`Method::Sync`] |
//! | Want Linux's `fdatasync` speedup | [`Method::Data`] |
//! | Have read-heavy random-access workloads | [`Method::Mmap`] |
//! | Need < 100 µs single-write latency on NVMe | [`Method::Direct`] |
//!
//! See [`docs/METHODS.md`](https://github.com/jamesgober/fsys-rs/blob/main/docs/METHODS.md)
//! for the full per-platform matrix and the `Auto` decision ladder.
//!
//! ## Crash safety
//!
//! Every write API (`write`, `write_copy`, `write_batch`,
//! `Batch::commit`) uses an atomic temp-file + rename pattern. The
//! target file is either entirely the old payload (kill before
//! rename) or entirely the new payload (kill after rename). Never
//! torn. See [`docs/CRASH-SAFETY.md`](https://github.com/jamesgober/fsys-rs/blob/main/docs/CRASH-SAFETY.md)
//! for the full per-method contract.
//!
//! ## Async (feature `async`)
//!
//! ```no_run
//! # async fn example() -> fsys::Result<()> {
//! # #[cfg(feature = "async")] {
//! let fs = std::sync::Arc::new(fsys::builder().build()?);
//! fs.clone().write_async("/tmp/async.dat", b"payload".to_vec()).await?;
//! let data = fs.clone().read_async("/tmp/async.dat").await?;
//! # }
//! # Ok(())
//! # }
//! ```
//!
//! Calling sync `fs.write()` from inside a tokio runtime is supported
//! (it just blocks the calling thread). Calling async
//! `fs.write_async()` outside a tokio runtime returns
//! [`Error::AsyncRuntimeRequired`] rather than panicking.

#![doc(html_root_url = "https://docs.rs/fsys/0.9.0")]
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

pub mod advice;
pub mod batch;
pub(crate) mod buffer;
pub mod builder;
pub mod crud;
pub mod error;
pub mod handle;
pub mod hardware;
pub mod journal;
pub mod meta;
pub mod method;
pub mod os;
pub mod path;
pub(crate) mod pipeline;
pub(crate) mod platform;
pub mod primitive;
pub mod quick;
pub mod substrate;

#[cfg(feature = "async")]
pub mod async_io;

/// Internal fuzz-test surface. Wraps `pub(crate)` helpers under
/// `cfg(feature = "fuzz")` so the cargo-fuzz workspace can reach
/// them without making them part of the public 1.0 API surface.
/// Subject to change without semver guarantees; do not use from
/// non-fuzz code.
#[cfg(feature = "fuzz")]
#[doc(hidden)]
pub mod __fuzz {
    use crate::journal::format as fmt;

    /// Public mirror of `crate::journal::format::FrameDecode` for
    /// fuzz harness consumption.
    #[derive(Debug)]
    pub enum FrameDecode {
        Ok {
            consumed: usize,
            payload_start: usize,
            payload_end: usize,
        },
        Truncated,
        BadMagic,
        LengthOverflow,
        ChecksumMismatch,
    }

    impl From<fmt::FrameDecode> for FrameDecode {
        fn from(d: fmt::FrameDecode) -> Self {
            match d {
                fmt::FrameDecode::Ok {
                    consumed,
                    payload_start,
                    payload_end,
                } => FrameDecode::Ok {
                    consumed,
                    payload_start,
                    payload_end,
                },
                fmt::FrameDecode::Truncated => FrameDecode::Truncated,
                fmt::FrameDecode::BadMagic => FrameDecode::BadMagic,
                fmt::FrameDecode::LengthOverflow => FrameDecode::LengthOverflow,
                fmt::FrameDecode::ChecksumMismatch => FrameDecode::ChecksumMismatch,
            }
        }
    }

    /// Encode a payload as a journal frame; returns the encoded
    /// `Vec<u8>`.
    pub fn encode_frame_owned(payload: &[u8]) -> crate::Result<Vec<u8>> {
        fmt::encode_frame_owned(payload)
    }

    /// Decode a journal frame from `bytes`.
    pub fn decode_frame(bytes: &[u8]) -> FrameDecode {
        fmt::decode_frame(bytes).into()
    }

    /// Compute the CRC-32C checksum of `bytes`.
    pub fn crc32c(bytes: &[u8]) -> u32 {
        fmt::crc32c(bytes)
    }
}

pub use crate::advice::Advice;
pub use crate::batch::Batch;
pub use crate::builder::Builder;
pub use crate::error::{BatchError, Error, Result};
pub use crate::handle::Handle;
pub use crate::journal::{
    JournalHandle, JournalOptions, JournalReader, JournalRecord, JournalTailState, Lsn,
};
pub use crate::meta::{DirEntry, FileMeta, Permissions};
pub use crate::method::Method;
pub use crate::path::Mode;
pub use crate::substrate::AsyncSubstrate;

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
