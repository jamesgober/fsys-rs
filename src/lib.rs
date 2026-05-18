//! # fsys
//!
//! Foundation-tier filesystem IO for Rust storage engines: journal
//! substrate, io_uring, NVMe passthrough, atomic writes, cross-platform
//! durability.
//!
//! `fsys` sits one layer below your data structures and one layer above
//! [`std::fs`]. It pairs an explicit durability model (you choose a
//! [`Method`], you get the platform's best matching primitive, you
//! observe any fallback via [`Handle::active_durability_primitive`])
//! with a journal substrate built for write-ahead-log workloads.
//!
//! ## Quickstart
//!
//! Append-only journal — the canonical WAL pattern:
//!
//! ```no_run
//! # fn example() -> fsys::Result<()> {
//! use std::sync::Arc;
//!
//! let fs = Arc::new(fsys::builder().build()?);
//! let log = fs.journal("/var/lib/myapp/log.wal")?;
//!
//! let _ = log.append(b"txn 1: insert")?;
//! let _ = log.append(b"txn 2: update")?;
//! let lsn = log.append(b"txn 3: commit")?;
//!
//! // One fsync covers every prior append — group-commit.
//! log.sync_through(lsn)?;
//! # Ok(())
//! # }
//! ```
//!
//! One-shot durable file write, no handle required:
//!
//! ```no_run
//! # fn example() -> fsys::Result<()> {
//! fsys::quick::write("/etc/myapp/config.toml", b"value = 42")?;
//! let data = fsys::quick::read("/etc/myapp/config.toml")?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Three tiers of API
//!
//! ### Tier 1 — one-shot helpers
//!
//! For programs that issue one IO op and don't need a long-lived handle.
//! Backed by a lazily-initialised default [`Handle`] with
//! [`Method::Auto`].
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
//! The primary API for everything beyond one-shot use. Build a [`Handle`]
//! with [`new()`] (default [`Method::Auto`]) or [`with(method)`](with);
//! share across threads via [`Arc`](std::sync::Arc).
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
//! For advanced configuration: custom root, dev/prod mode, per-handle
//! batch knobs, io_uring queue depth, buffer pool size, observer hook,
//! workload presets.
//!
//! ```no_run
//! # fn example() -> fsys::Result<()> {
//! let fs = fsys::builder()
//!     .method(fsys::Method::Direct)
//!     .root("/var/lib/myapp")
//!     .mode(fsys::Mode::Prod)
//!     .tune_for(fsys::Workload::Database)
//!     .build()?;
//! # Ok(())
//! # }
//! ```
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
//! for the full per-platform matrix and the [`Method::Auto`] decision ladder.
//!
//! ## Crash safety
//!
//! Every write API (`write`, `write_copy`, `write_batch`,
//! `Batch::commit`) uses an atomic temp-file + rename pattern. The target
//! file is either entirely the old payload (kill before rename) or
//! entirely the new payload (kill after rename) — never torn.
//! The [`journal`] substrate adds explicit durability via
//! [`JournalHandle::sync_through`]; group-commit amortises the fsync
//! cost across many appends. See
//! [`docs/CRASH-SAFETY.md`](https://github.com/jamesgober/fsys-rs/blob/main/docs/CRASH-SAFETY.md)
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
//! On Linux + [`Method::Direct`], async ops submit directly to the
//! per-handle io_uring ring — the *native substrate*, observable via
//! [`Handle::async_substrate`] returning [`AsyncSubstrate::NativeIoUring`].
//! Everywhere else, async ops route through `tokio::task::spawn_blocking`
//! ([`AsyncSubstrate::SpawnBlocking`]).
//!
//! Calling sync `fs.write()` from inside a tokio runtime is supported
//! (it just blocks the calling thread). Calling async `fs.write_async()`
//! outside a tokio runtime returns [`Error::AsyncRuntimeRequired`]
//! rather than panicking.
//!
//! ## Cargo features
//!
//! | Feature | Default | Purpose |
//! |---|---|---|
//! | `async` | off | `_async` siblings for every sync method; pulls in `tokio` |
//! | `tracing` | off | Structured spans + events on the write / read / journal hot paths |
//! | `stress` | off | Run `tests/stress.rs` for the full 1-hour soak (CI-nightly only) |
//! | `fuzz` | off | Compile-only flag for fuzz-target wiring; targets live in `fuzz/` |
//!
//! ## Concept reference
//!
//! | Concept | Type / module | Reach for it when... |
//! |---|---|---|
//! | Handle to filesystem | [`Handle`] | Any non-one-shot IO. The primary type. |
//! | Configure handle | [`Builder`] | Custom root, method, tuning, observer. |
//! | Durability strategy | [`Method`] | Five variants: `Sync`, `Data`, `Mmap`, `Direct`, `Auto`. |
//! | Append-only WAL | [`JournalHandle`] | High-throughput durable writes (WAL / ledger / queue). |
//! | Multi-op transaction | [`Batch`] | Group N writes / deletes / copies under one durability barrier. |
//! | One-shot helpers | [`mod@quick`] | Single-call file IO without holding a handle. |
//! | Async layer | `fsys::async_io` (feature `async`) | Tokio integration; `_async` siblings for every sync method. |
//! | Telemetry hook | [`observer::FsysObserver`] | Per-op events (append / sync / write / read). |
//! | Hardware introspection | [`mod@hardware`] | Probe PLP status, atomic-write unit, sector size. |
//! | Errors | [`Error`] / [`Result`] | 21 variants with stable `FS-XXXXX` codes. |
//!
//! ## Version history
//!
//! Per-version deltas live in
//! [`CHANGELOG.md`](https://github.com/jamesgober/fsys-rs/blob/main/CHANGELOG.md).
//! `1.0.0` is the first stable release; the `1.x` line is API-stable
//! and on-disk-format-stable per the contract in
//! [`docs/STABILITY-1.0.md`](https://github.com/jamesgober/fsys-rs/blob/main/docs/STABILITY-1.0.md).

// 0.9.6 audit H-12 — `html_root_url` was previously pinned to a
// specific version string that drifted every release. docs.rs
// handles per-version routing automatically; pinning it here
// just creates a fix-at-version-bump chore that we'd inevitably
// forget. Removed entirely. If a future release needs to
// override docs.rs's default routing (rare), the attribute can
// be re-added at that point with a clear reason.
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
pub mod capability;
pub mod crud;
pub mod error;
pub mod handle;
pub mod hardware;
pub mod journal;
pub mod meta;
pub mod method;
pub mod observer;
pub mod os;
pub mod path;
pub(crate) mod pipeline;
pub(crate) mod platform;
pub mod primitive;
pub mod quick;
pub mod substrate;

#[cfg(feature = "async")]
pub mod async_io;

/// 0.9.7 H-7 — internal OOM-injection allocator hooks.
///
/// **NEVER enable the `oom_inject` feature in production.** Every
/// allocation pays a thread-local lookup + comparison. The
/// feature exists solely to make `tests/oom_injection.rs`
/// compileable; the integration test is the regression guard for
/// `AlignedBuf::new` / `read_all_direct` / other fallible-alloc
/// paths.
///
/// When `oom_inject` is enabled, the global allocator is replaced
/// with `OomInjectingAllocator` from `test_support`. The
/// `OOM_THRESHOLD` thread-local controls injection: allocations
/// of `>= threshold` bytes return `null` (OOM). Tests use the
/// `OomThreshold` RAII guard to set + restore the threshold
/// safely across scope exit (including panics).
#[cfg(feature = "oom_inject")]
#[doc(hidden)]
pub mod test_support;

#[cfg(feature = "oom_inject")]
#[global_allocator]
static FSYS_OOM_INJECTING_ALLOCATOR: test_support::OomInjectingAllocator =
    test_support::OomInjectingAllocator;

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
pub use crate::builder::{Builder, SpdkConfig, Workload};
pub use crate::capability::{
    Capabilities, HardwareSummary, IoUringFeature, PciAddress, SpdkEligibility, SpdkSkipReason,
};
pub use crate::error::{BatchError, Error, Result};
pub use crate::handle::Handle;
pub use crate::journal::{
    JournalBackend, JournalBackendHealth, JournalBackendInfo, JournalBackendKind, JournalHandle,
    JournalOptions, JournalReader, JournalRecord, JournalTailState, Lsn, SyncMode,
    WriteLifetimeHint,
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
