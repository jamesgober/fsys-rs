//! Canonical names for the durability primitives that fsys actually
//! invokes under the hood, returned by
//! [`crate::Handle::active_durability_primitive`].
//!
//! These are the exact strings reported at runtime; users match
//! against them with the constants defined here to avoid string-typo
//! bugs:
//!
//! ```
//! use fsys::primitive;
//!
//! # let primitive = primitive::FSYNC;
//! match primitive {
//!     primitive::IO_URING_NVME_FLUSH => println!("NVMe passthrough path"),
//!     primitive::IO_URING_FDATASYNC => println!("io_uring + fdatasync path"),
//!     primitive::FSYNC => println!("fallback fsync path"),
//!     _ => println!("other primitive"),
//! }
//! ```
//!
//! ## Stability
//!
//! The set of strings is stable across `0.x.y` releases. New
//! primitives may be added in minor versions; the existing strings
//! never change for an existing primitive. `0.6.0` adds the NVMe
//! passthrough variants.
//!
//! ## Per-platform mapping
//!
//! - **Linux:** `IO_URING_NVME_FLUSH` (kernel ≥ 5.19, NVMe access),
//!   `IO_URING_FDATASYNC` (kernel ≥ 5.1), `O_DIRECT_PWRITE_FDATASYNC`
//!   (fallback), `FDATASYNC` (Method::Data),
//!   `FSYNC` (Method::Sync), `MMAP_MSYNC` (Method::Mmap).
//! - **macOS:** `F_NOCACHE_F_FULLFSYNC` (Method::Direct),
//!   `F_FULLFSYNC` (Method::Sync / Method::Data),
//!   `MMAP_MSYNC` (Method::Mmap).
//! - **Windows:** `FILE_FLAG_WRITE_THROUGH_NVME_IOCTL` (NVMe + admin),
//!   `FILE_FLAG_WRITE_THROUGH` (Method::Direct fallback),
//!   `FILE_FLAG_NO_BUFFERING_FLUSHFILEBUFFERS` (alternate Direct),
//!   `FSYNC` (FlushFileBuffers — Method::Sync / Method::Data),
//!   `MMAP_MSYNC` (Method::Mmap).

#![allow(missing_docs)] // each constant is self-documenting via its name

/// Linux io_uring submission with `IORING_OP_URING_CMD` carrying NVMe
/// FLUSH opcode `0x00`. The "elite" Linux Direct path on hardware +
/// permissions that allow raw NVMe commands.
pub const IO_URING_NVME_FLUSH: &str = "io_uring + NVMe FLUSH";

/// Linux io_uring submission with `IORING_OP_FSYNC + DATASYNC`. Used
/// when io_uring is available but NVMe passthrough is not.
pub const IO_URING_FDATASYNC: &str = "io_uring + fdatasync";

/// Linux fallback when io_uring is unavailable: synchronous `pwrite`
/// on an `O_DIRECT` fd followed by `fdatasync(2)`.
pub const O_DIRECT_PWRITE_FDATASYNC: &str = "O_DIRECT + pwrite + fdatasync";

/// macOS `Method::Direct`: cache bypass via `F_NOCACHE` plus
/// `F_FULLFSYNC` for media-flush durability.
pub const F_NOCACHE_F_FULLFSYNC: &str = "F_NOCACHE + F_FULLFSYNC";

/// macOS `Method::Sync` / `Method::Data`: `F_FULLFSYNC` (regular
/// `fsync` does not flush to media on macOS).
pub const F_FULLFSYNC: &str = "F_FULLFSYNC";

/// Windows `Method::Direct` with NVMe IOCTL: `FILE_FLAG_WRITE_THROUGH`
/// plus `IOCTL_STORAGE_PROTOCOL_COMMAND` carrying NVMe FLUSH.
pub const FILE_FLAG_WRITE_THROUGH_NVME_IOCTL: &str = "FILE_FLAG_WRITE_THROUGH + NVMe IOCTL";

/// Windows `Method::Direct` fallback: `FILE_FLAG_WRITE_THROUGH` only.
pub const FILE_FLAG_WRITE_THROUGH: &str = "FILE_FLAG_WRITE_THROUGH";

/// Windows alternate Direct path: `FILE_FLAG_NO_BUFFERING` plus
/// explicit `FlushFileBuffers`. (Not the default — `WRITE_THROUGH` is
/// preferred — but exposed for completeness.)
pub const FILE_FLAG_NO_BUFFERING_FLUSHFILEBUFFERS: &str =
    "FILE_FLAG_NO_BUFFERING + FlushFileBuffers";

/// Linux `Method::Sync` / Windows `Method::Sync`: classic
/// `fsync(2)` / `FlushFileBuffers`.
pub const FSYNC: &str = "fsync";

/// Linux `Method::Data`: classic `fdatasync(2)`. (On Windows / macOS,
/// `Method::Data` falls back to [`FSYNC`] / [`F_FULLFSYNC`]
/// respectively.)
pub const FDATASYNC: &str = "fdatasync";

/// `Method::Mmap`: `mmap` followed by `msync(MS_SYNC)` / Windows
/// `FlushViewOfFile`.
pub const MMAP_MSYNC: &str = "mmap + msync";
