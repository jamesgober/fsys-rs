//! Access-pattern hints for the kernel page cache.
//!
//! [`Advice`] is the public-API enum that callers pass to
//! [`JournalHandle::advise`](crate::JournalHandle::advise) or
//! [`JournalReader::advise`](crate::JournalReader::advise) to
//! inform the kernel about expected access patterns. The kernel
//! uses these hints to drive page-cache prefetch, eviction, and
//! read-ahead policy.
//!
//! Hints are *advisory* — the OS is free to ignore them. They
//! never affect correctness; they only affect performance. Use
//! them when the access pattern is known up-front and predictable;
//! omit them when the pattern is unknown (the OS's adaptive
//! defaults are usually fine).
//!
//! # Platform mapping
//!
//! | `Advice`       | Linux              | macOS                                | Windows           |
//! |----------------|--------------------|--------------------------------------|-------------------|
//! | `Sequential`   | `POSIX_FADV_SEQUENTIAL` | `F_RDADVISE` (full file)         | best-effort       |
//! | `Random`       | `POSIX_FADV_RANDOM`     | (no equivalent — best-effort)    | best-effort       |
//! | `WillNeed`     | `POSIX_FADV_WILLNEED`   | `F_RDADVISE`                     | best-effort       |
//! | `DontNeed`     | `POSIX_FADV_DONTNEED`   | `fcntl(F_NOCACHE, 1)` then back  | best-effort       |
//! | `Normal`       | `POSIX_FADV_NORMAL`     | clears prior hints               | best-effort       |
//!
//! "best-effort" on Windows means the operation succeeds without
//! actually issuing a hint — Windows lacks a per-range cache
//! advisory API. Sequential / random hints CAN be applied at
//! file-open time via `FILE_FLAG_SEQUENTIAL_SCAN` /
//! `FILE_FLAG_RANDOM_ACCESS`, but only at open and only at the
//! whole-file granularity. The runtime `advise` calls on Windows
//! are no-ops that succeed.

/// Access-pattern advice for a file region.
///
/// Pass to [`crate::JournalHandle::advise`] or
/// [`crate::JournalReader::advise`] to inform the kernel about
/// expected access. The kernel uses the hint to optimise
/// page-cache behaviour (prefetch, eviction, read-ahead window
/// size).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Advice {
    /// "I'll read this region linearly from start to end."
    /// The kernel typically extends its read-ahead window —
    /// good for sequential scans like journal replay or
    /// full-table scans.
    Sequential,
    /// "I'll read this region in a non-predictable order."
    /// The kernel typically reduces or disables read-ahead —
    /// good for B-tree page reads or random-access workloads.
    Random,
    /// "I'll need this region soon." The kernel may begin
    /// asynchronous prefetch into the page cache. Useful for
    /// warming the cache before a hot path enters its critical
    /// section.
    WillNeed,
    /// "I'm done with this region — feel free to evict."
    /// Releases pages from the page cache so they can be
    /// repurposed for other data. Critical for streaming write
    /// workloads where the data being written won't be re-read
    /// (avoids polluting the page cache with bytes that will
    /// never be touched again).
    DontNeed,
    /// "Use the OS's default policy." Resets any prior hint on
    /// this region.
    Normal,
}
