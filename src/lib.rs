//! # fsys
//!
//! Adaptive file and directory IO for Rust — fast, hardware-aware, multi-strategy.
//!
//! `fsys` is a low-level filesystem abstraction designed for storage engines,
//! databases, and any application that needs predictable, high-performance
//! file IO with explicit control over durability strategy.
//!
//! ## Status
//!
//! This crate is in early development. The public API is not yet stable.
//!
//! ## Goals
//!
//! - **Hardware-aware.** Detect drive type (NVMe, SSD, HDD) and capabilities
//!   (PLP, queue depth, optimal block size) at startup. Pick sensible defaults.
//! - **Multi-strategy durability.** Support `fsync`, `fdatasync`, NVMe passthrough
//!   flush, and Phantom Buffer modes. Let the caller pick the right tradeoff.
//! - **Adaptive dispatch.** Route single writes through a low-latency solo lane
//!   and bulk writes through a group-commit lane. Switching is automatic.
//! - **Cross-platform.** Linux, macOS, Windows. Best path on each.
//! - **Zero magic.** Every strategy is explicit. No hidden buffering, no
//!   unexpected flushes, no surprises in the latency tail.
//!
//! ## Non-goals
//!
//! - High-level filesystem features (locking, watch APIs, fancy paths).
//! - Replacing `std::fs` for general use.
//! - Async-runtime integration (this is a synchronous core; async wrappers
//!   may live in a separate crate).

#![doc(html_root_url = "https://docs.rs/fsys/0.1.0")]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(rust_2018_idioms)]
#![warn(clippy::all)]

/// Library version, matching the crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
