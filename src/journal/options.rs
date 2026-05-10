//! [`JournalOptions`] — opt-in configuration for [`JournalHandle`].
//!
//! The default configuration is the lock-free buffered path
//! (no Direct IO, no internal log buffer — every append is a
//! `pwrite` directly against the per-handle file). Direct-IO
//! mode is opted into per-journal via [`JournalOptions::direct`].

use crate::journal::JournalHandle;
use crate::Result;
use std::path::Path;
use std::time::Duration;

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

/// Default group-commit batching window: leader waits **at most**
/// this long for followers to join before issuing the fsync.
/// Ported from emdb v0.8.5's group-commit coordinator, where this
/// value (500 µs) achieved 8× aggregate write throughput on a
/// 4-core consumer box with 8 producer threads. `None` means "no
/// window" — the leader fsyncs immediately on first call, matching
/// pre-0.9.1 behaviour.
pub(crate) const DEFAULT_GROUP_COMMIT_WINDOW: Option<Duration> = Some(Duration::from_micros(500));

/// Default group-commit max-batch hint: the leader exits its
/// wait window early once this many followers have joined.
/// Ported from emdb v0.8.5 (`max_batch = 8` aligned with
/// `num_cpus::get()` on the original target hardware). The
/// hint is advisory — followers always coalesce regardless of
/// the hint; the hint only governs how long the leader is
/// willing to wait for additional ones.
pub(crate) const DEFAULT_GROUP_COMMIT_MAX_BATCH: u32 = 8;

/// Smallest accepted `group_commit_max_batch`. Setting it to 1
/// effectively disables batching (every leader fsyncs alone with
/// no wait); we accept it but document the effect.
pub(crate) const MIN_GROUP_COMMIT_MAX_BATCH: u32 = 1;

/// Largest accepted `group_commit_max_batch`. 4096 is well above
/// any realistic concurrent-flusher count on contemporary
/// hardware; the cap prevents pathological values from causing
/// unbounded waits.
pub(crate) const MAX_GROUP_COMMIT_MAX_BATCH: u32 = 4096;

/// Largest accepted `group_commit_window`. 100 ms is well above
/// the natural timescale of a database's commit cadence; the cap
/// prevents pathological values from holding up commits
/// indefinitely.
pub(crate) const MAX_GROUP_COMMIT_WINDOW: Duration = Duration::from_millis(100);

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
    /// Leader/follower group-commit batching window. The leader
    /// waits up to this long for followers to enqueue before
    /// issuing the fsync. `None` disables the window — the leader
    /// fsyncs immediately, matching pre-0.9.1 behaviour. New in
    /// 0.9.1; default `Some(500 µs)`.
    pub(crate) group_commit_window: Option<Duration>,
    /// Leader exit hint: stops waiting in `group_commit_window`
    /// once at least this many followers have joined. New in
    /// 0.9.1; default `8`.
    pub(crate) group_commit_max_batch: u32,
}

impl Default for JournalOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalOptions {
    /// Returns a fresh `JournalOptions` with library-default values:
    /// `direct = false`, `log_buffer_kib = 64`,
    /// `group_commit_window = Some(500 µs)`,
    /// `group_commit_max_batch = 8`. Equivalent to
    /// [`Default::default`].
    pub fn new() -> Self {
        Self {
            direct: false,
            log_buffer_kib: DEFAULT_LOG_BUFFER_KIB,
            group_commit_window: DEFAULT_GROUP_COMMIT_WINDOW,
            group_commit_max_batch: DEFAULT_GROUP_COMMIT_MAX_BATCH,
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

    /// Sets the leader/follower group-commit batching window.
    ///
    /// When [`crate::JournalHandle::sync_through`] is called by
    /// multiple threads concurrently, the first caller becomes
    /// the **leader**: it waits up to `window` for additional
    /// callers to enqueue, then issues a single `fdatasync`
    /// covering everyone's LSNs. Followers parking on the
    /// condvar wake immediately when the fsync completes.
    ///
    /// **`Some(d)` enables batching**, with the leader holding
    /// for at most `d` (clamped to the range
    /// `0 µs..=100 ms`). Setting `d` to zero is the same as
    /// passing `None`.
    ///
    /// **`None` disables batching** — the leader fsyncs as soon
    /// as it acquires the gate. Concurrent followers still
    /// coalesce around the **in-flight** fsync (no need to
    /// re-fsync if the in-flight sync's frontier already covers
    /// their target LSN).
    ///
    /// **Default: `Some(500 µs)`.** Ported from emdb v0.8.5's
    /// group-commit coordinator, which achieved 8× aggregate
    /// write throughput vs unbatched per-flush mode at this
    /// setting. New in 0.9.1; pre-0.9.1 behaviour was equivalent
    /// to `None`.
    pub fn group_commit_window(mut self, window: Option<Duration>) -> Self {
        self.group_commit_window = match window {
            Some(d) if d.is_zero() => None,
            Some(d) if d > MAX_GROUP_COMMIT_WINDOW => Some(MAX_GROUP_COMMIT_WINDOW),
            other => other,
        };
        self
    }

    /// Sets the maximum number of follower flushers a leader
    /// will wait for before exiting its
    /// [`Self::group_commit_window`] early.
    ///
    /// The leader exits the wait window as soon as either:
    /// 1. The window elapses, or
    /// 2. At least this many followers have joined.
    ///
    /// **Tune to your concurrent-flusher count.** Setting this
    /// higher than the realistic concurrent flusher count is a
    /// performance trap — the leader waits the full window for
    /// followers that never arrive, turning the window into
    /// pure tail latency. The emdb v0.8.5 documentation calls
    /// this out explicitly; we preserve the same advice here.
    /// As a rule of thumb, set to `num_cpus::get()` for general
    /// server workloads.
    ///
    /// Clamped to `[1, 4096]`. Setting it to `1` effectively
    /// disables batching (every leader fsyncs alone with no
    /// wait). **Default: `8`.** New in 0.9.1.
    pub fn group_commit_max_batch(mut self, max_batch: u32) -> Self {
        self.group_commit_max_batch =
            max_batch.clamp(MIN_GROUP_COMMIT_MAX_BATCH, MAX_GROUP_COMMIT_MAX_BATCH);
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

    // ─────────────────────────────────────────────────────────
    // 0.9.1 — group-commit knob coverage
    // ─────────────────────────────────────────────────────────

    #[test]
    fn defaults_match_expected_group_commit() {
        let o = JournalOptions::default();
        assert_eq!(o.group_commit_window, DEFAULT_GROUP_COMMIT_WINDOW);
        assert_eq!(o.group_commit_max_batch, DEFAULT_GROUP_COMMIT_MAX_BATCH);
    }

    #[test]
    fn group_commit_window_zero_becomes_none() {
        let o = JournalOptions::new().group_commit_window(Some(Duration::ZERO));
        assert_eq!(o.group_commit_window, None);
    }

    #[test]
    fn group_commit_window_above_cap_is_clamped() {
        let huge = Duration::from_secs(60);
        let o = JournalOptions::new().group_commit_window(Some(huge));
        assert_eq!(o.group_commit_window, Some(MAX_GROUP_COMMIT_WINDOW));
    }

    #[test]
    fn group_commit_window_in_range_passes_through() {
        let d = Duration::from_micros(750);
        let o = JournalOptions::new().group_commit_window(Some(d));
        assert_eq!(o.group_commit_window, Some(d));
    }

    #[test]
    fn group_commit_window_none_disables_batching() {
        let o = JournalOptions::new().group_commit_window(None);
        assert_eq!(o.group_commit_window, None);
    }

    #[test]
    fn group_commit_max_batch_clamped() {
        let o = JournalOptions::new().group_commit_max_batch(0);
        assert_eq!(o.group_commit_max_batch, MIN_GROUP_COMMIT_MAX_BATCH);

        let o = JournalOptions::new().group_commit_max_batch(u32::MAX);
        assert_eq!(o.group_commit_max_batch, MAX_GROUP_COMMIT_MAX_BATCH);

        let o = JournalOptions::new().group_commit_max_batch(64);
        assert_eq!(o.group_commit_max_batch, 64);
    }
}
