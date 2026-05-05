//! [`JournalOptions`] — opt-in configuration for [`JournalHandle`].
//!
//! The default configuration is the lock-free buffered path
//! (no Direct IO, no internal log buffer — every append is a
//! `pwrite` directly against the per-handle file). Direct-IO
//! mode is opted into per-journal via [`JournalOptions::direct`].

use crate::journal::JournalHandle;
use crate::Result;
use std::path::Path;

/// Default in-memory log buffer size for `JournalOptions::direct(true)`.
/// Holds buffered records before they're flushed to disk in
/// sector-aligned chunks. 64 KiB is large enough that a single flush
/// covers many small WAL records (mirroring InnoDB's 16 KiB default
/// log buffer × 4) without occupying excessive per-journal memory.
///
/// Callers can raise this via [`JournalOptions::log_buffer_kib`] for
/// burst-heavy workloads where the cost of an extra flush dominates.
pub(crate) const DEFAULT_LOG_BUFFER_KIB: u32 = 64;

/// Smallest log buffer size accepted. One sector is the absolute
/// minimum that still produces sector-aligned writes; we set the
/// floor at 4 KiB to match the typical NVMe sector size and avoid
/// pathological one-record-per-flush configurations.
pub(crate) const MIN_LOG_BUFFER_BYTES: u32 = 4 * 1024;

/// Largest log buffer size accepted. 64 MiB is two orders of
/// magnitude above any realistic WAL configuration; the cap prevents
/// runaway memory usage from an out-of-bounds caller value.
pub(crate) const MAX_LOG_BUFFER_BYTES: u32 = 64 * 1024 * 1024;

/// Per-journal opt-in configuration.
///
/// Construct via [`JournalOptions::new`] (or [`Default::default`])
/// and pass to [`crate::Handle::journal_with`]. The default is
/// the buffered, lock-free `pwrite` path; opting into
/// [`JournalOptions::direct`] switches the journal to a
/// sector-aligned log-buffer architecture analogous to the redo-
/// log buffers used by InnoDB and WiredTiger.
///
/// # Example
///
/// ```no_run
/// use fsys::{builder, JournalOptions};
///
/// # fn main() -> fsys::Result<()> {
/// let fs = builder().build()?;
/// let log = fs.journal_with(
///     "/var/lib/mydb/wal",
///     JournalOptions::new().direct(true).log_buffer_kib(256),
/// )?;
/// # let _ = log;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
#[must_use]
pub struct JournalOptions {
    pub(crate) direct: bool,
    pub(crate) log_buffer_kib: u32,
}

impl Default for JournalOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalOptions {
    /// Returns a fresh `JournalOptions` with library-default values:
    /// `direct = false`, `log_buffer_kib = 64`. Equivalent to
    /// [`Default::default`].
    pub fn new() -> Self {
        Self {
            direct: false,
            log_buffer_kib: DEFAULT_LOG_BUFFER_KIB,
        }
    }

    /// Opts the journal into Direct-IO mode.
    ///
    /// When enabled, the journal file is opened with `O_DIRECT`
    /// (Linux) / `F_NOCACHE` (macOS) /  `FILE_FLAG_NO_BUFFERING`
    /// (Windows). Every append is serialised into an in-memory
    /// sector-aligned log buffer; full buffers (and
    /// [`crate::JournalHandle::sync_through`] callers) flush to
    /// disk via a single sector-aligned positioned write. This
    /// eliminates the page-cache hop that the buffered path
    /// incurs — the kernel writes user-space bytes directly into
    /// device DMA — at the cost of mutex-serialised appends in
    /// place of the lock-free fast path.
    ///
    /// **When to enable it.** Use Direct-IO mode when:
    /// - You're building a database / queue / ledger whose WAL
    ///   throughput depends on saturating NVMe device bandwidth
    ///   without paying the page-cache memcpy on every record.
    /// - Your workload is dominated by sustained sequential
    ///   appends (the log-buffer batch coalesces many small
    ///   records into one sector-aligned write).
    /// - You're measuring tail latency: bypassing the page cache
    ///   removes a class of jitter caused by background cache
    ///   pressure.
    ///
    /// **When to leave it off (the default).** The buffered path
    /// is faster for:
    /// - Bursty workloads with frequent fsync-per-record
    ///   semantics: the page cache absorbs the burst and the
    ///   write-back is amortised.
    /// - Workloads where individual records are the size of a
    ///   sector or larger (no batching benefit, full memcpy
    ///   penalty).
    /// - Smoke tests / development. The buffered path's
    ///   lock-free append scales linearly with thread count.
    ///
    /// # Platform support
    ///
    /// - **Linux** — `O_DIRECT`. Falls back to buffered if the
    ///   filesystem rejects it (tmpfs, FUSE, certain CIFS
    ///   mounts). The fallback is observable via
    ///   [`JournalHandle::is_direct_active`].
    /// - **macOS** — `F_NOCACHE`. Always available on local
    ///   volumes.
    /// - **Windows** — `FILE_FLAG_NO_BUFFERING`. Always
    ///   available; the journal file is rejected on certain
    ///   network filesystems.
    /// - **Other** — silently equivalent to `direct(false)`. The
    ///   knob compiles cleanly; the journal uses buffered IO.
    pub fn direct(mut self, yes: bool) -> Self {
        self.direct = yes;
        self
    }

    /// Sets the in-memory log buffer size (in KiB) for Direct-IO
    /// mode. Ignored when `direct = false`.
    ///
    /// Clamped to `[4, 65 536]` KiB. The default is 64 KiB.
    ///
    /// **Larger buffers** amortise the cost of group-commit
    /// fsyncs across more records (better sustained throughput,
    /// higher worst-case latency before a flush triggers).
    /// **Smaller buffers** trigger more frequent flushes (lower
    /// latency-per-record at peak, lower aggregate throughput on
    /// burst workloads).
    pub fn log_buffer_kib(mut self, kib: u32) -> Self {
        self.log_buffer_kib = kib.clamp(MIN_LOG_BUFFER_BYTES / 1024, MAX_LOG_BUFFER_BYTES / 1024);
        self
    }
}

/// Internal: opens a journal honoring `options`. Called by
/// [`crate::Handle::journal`] (default options) and
/// [`crate::Handle::journal_with`] (caller-supplied options).
pub(crate) fn open_with_options(path: &Path, options: JournalOptions) -> Result<JournalHandle> {
    JournalHandle::open_with_options(path, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_buffered_64k() {
        let o = JournalOptions::default();
        assert!(!o.direct);
        assert_eq!(o.log_buffer_kib, DEFAULT_LOG_BUFFER_KIB);
    }

    #[test]
    fn direct_toggle_round_trips() {
        let o = JournalOptions::new().direct(true);
        assert!(o.direct);
        let o = o.direct(false);
        assert!(!o.direct);
    }

    #[test]
    fn log_buffer_kib_is_clamped() {
        // Below floor: clamped to 4 KiB.
        let o = JournalOptions::new().log_buffer_kib(0);
        assert_eq!(o.log_buffer_kib, MIN_LOG_BUFFER_BYTES / 1024);

        // Above ceiling: clamped to 64 MiB.
        let o = JournalOptions::new().log_buffer_kib(u32::MAX);
        assert_eq!(o.log_buffer_kib, MAX_LOG_BUFFER_BYTES / 1024);

        // In range: pass-through.
        let o = JournalOptions::new().log_buffer_kib(256);
        assert_eq!(o.log_buffer_kib, 256);
    }
}
