//! Async journal API — `_async` siblings of [`JournalHandle`]'s
//! sync surface.
//!
//! ## Substrate selection
//!
//! - **Linux + `async` feature, io_uring available** — uses the
//!   native io_uring substrate (tier-3 of R-1). Each
//!   `append_async` call submits an `IORING_OP_WRITE` SQE through
//!   the per-journal `AsyncIoUring` ring; each
//!   `sync_through_async` submits an
//!   `IORING_OP_FSYNC(DATASYNC)` SQE. No `spawn_blocking` hop —
//!   the calling tokio task `.await`s a `oneshot` driven by the
//!   ring's completion driver. This is the path that delivers
//!   millions of durable ops/sec on bare-metal Linux + NVMe.
//! - **Linux + `async` feature, io_uring unavailable** — falls
//!   back to `spawn_blocking` against the sync API. `OnceLock`
//!   caches the construction failure so subsequent ops don't
//!   re-attempt.
//! - **macOS / Windows** — `spawn_blocking` is the only async
//!   substrate available; same fallback.
//!
//! The substrate is selected per-journal and is observable via
//! [`JournalHandle::native_iouring_active`] (Linux + async only).
//!
//! ## Same guarantees on every substrate
//!
//! The native path follows the sync journal's protocol: a group-commit
//! leader (sync or async) only publishes a durable frontier once every
//! append reserved below it has been written, and a failed write or
//! fsync poisons the journal (see [`JournalHandle::sync_through`]).
//! Both hold when an `_async` future is dropped part-way: a write or
//! fsync that was already queued finishes in the background and its
//! outcome is still applied to the journal.
//!
//! A blocking [`JournalHandle::sync_through`] leader waits for queued
//! native async writes, which complete on the io_uring owner task. Do
//! not call it on a runtime worker thread of the runtime that runs
//! the journal's async ops (use `sync_through_async`, or
//! `spawn_blocking`); on a single-threaded runtime that would wait
//! forever.

use crate::journal::{JournalHandle, Lsn};
use crate::{Error, Result};
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[cfg(all(target_os = "linux", feature = "async"))]
use crate::async_io::completion_driver::{AsyncIoUring, FileRef, IoBuf, OpHook};
#[cfg(all(target_os = "linux", feature = "async"))]
use crate::journal::gate::DetachedTicket;
#[cfg(all(target_os = "linux", feature = "async"))]
use parking_lot::Mutex;
#[cfg(all(target_os = "linux", feature = "async"))]
use std::sync::atomic::AtomicBool;

/// Default queue depth for journal-owned io_uring rings on Linux.
/// 256 is large enough to absorb burst append load without backpressure
/// for typical workloads (database WAL, persistent queue) and small
/// enough that the ring's kernel-side memory footprint stays under
/// 64 KiB per journal.
#[cfg(all(target_os = "linux", feature = "async"))]
const JOURNAL_IOURING_DEPTH: u32 = 256;

impl JournalHandle {
    /// Async variant of [`JournalHandle::append`].
    ///
    /// On Linux + io_uring available: submits `IORING_OP_WRITE`
    /// at the LSN-reserved offset and `.await`s the CQ
    /// completion. No `spawn_blocking` hop. On other platforms or
    /// when io_uring is unavailable, falls back to
    /// `spawn_blocking` against the sync `append` method.
    ///
    /// The owned `record: Vec<u8>` is moved into the future
    /// because both substrates require `'static` payloads (the
    /// io_uring SQE captures the buffer pointer/length until the
    /// CQE arrives; `spawn_blocking` requires `'static` for its
    /// closure).
    ///
    /// # Errors
    ///
    /// - [`Error::AsyncRuntimeRequired`] if called outside a tokio
    ///   runtime.
    /// - [`Error::Io`] on the underlying append failure, which
    ///   poisons the journal, or with the poison error if an earlier
    ///   write, flush or fsync failed (same rules as
    ///   [`JournalHandle::append`]).
    pub async fn append_async(self: Arc<Self>, record: Vec<u8>) -> Result<Lsn> {
        super::require_runtime()?;
        // Empty records produce a valid 12-byte framed marker
        // (length=0). Don't short-circuit — uniform framing is
        // load-bearing for the reader's tail-truncation
        // detection invariant.

        // Direct-IO journals route through the in-memory log
        // buffer (mutex-serialised), bypassing the lock-free
        // native io_uring write path — submitting raw frames at
        // LSN-reserved offsets would skip the sector-aligned
        // buffering invariant. Fall back to spawn_blocking so the
        // sync `append` path can take the buffer mutex.
        if self.direct {
            return tokio::task::spawn_blocking(move || self.append(&record))
                .await
                .map_err(join_error_to_io)?;
        }

        // Native io_uring fast path — Linux + ring constructible.
        // Buffered-mode journals only.
        #[cfg(all(target_os = "linux", feature = "async"))]
        if let Some(ring) = self.native_ring() {
            return self.append_native(ring, record).await;
        }
        // Cross-platform fallback — spawn_blocking against the sync API.
        tokio::task::spawn_blocking(move || self.append(&record))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`JournalHandle::sync_through`].
    ///
    /// On Linux + io_uring available: submits
    /// `IORING_OP_FSYNC(DATASYNC)` and `.await`s the CQ
    /// completion. Sync and async callers share one group-commit
    /// coordinator, so only one fsync (whether sync or async) is in
    /// flight at a time per journal, and the published frontier
    /// never covers an append that has not been written. On other
    /// platforms or when io_uring is unavailable, falls back to
    /// `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// - [`Error::AsyncRuntimeRequired`] if called outside a tokio
    ///   runtime.
    /// - [`Error::Io`] on the underlying fsync failure, which
    ///   poisons the journal, or with the poison error if an earlier
    ///   write, flush or fsync failed and `lsn` is not already
    ///   durable (same rules as [`JournalHandle::sync_through`]).
    pub async fn sync_through_async(self: Arc<Self>, lsn: Lsn) -> Result<()> {
        super::require_runtime()?;
        let lsn_off = lsn.as_u64();
        // Fast path: already synced.
        if self.synced_lsn.load(Ordering::Acquire) >= lsn_off {
            return Ok(());
        }
        // Direct-IO journals: the sync path must flush the
        // in-memory log buffer's partial trailing sector before
        // fdatasync. The native sync_through_native only submits
        // IORING_OP_FSYNC and would miss the buffer flush. Fall
        // back to spawn_blocking so the sync path takes the
        // buffer mutex and runs flush_partial + fdatasync.
        if self.direct {
            return tokio::task::spawn_blocking(move || self.sync_through(lsn))
                .await
                .map_err(join_error_to_io)?;
        }
        // Native io_uring fast path — buffered-mode journals only.
        #[cfg(all(target_os = "linux", feature = "async"))]
        if let Some(ring) = self.native_ring() {
            return self.sync_through_native(ring, lsn).await;
        }
        // Cross-platform fallback.
        tokio::task::spawn_blocking(move || self.sync_through(lsn))
            .await
            .map_err(join_error_to_io)?
    }

    /// Returns whether this journal's async substrate is using the
    /// native io_uring path (Linux + `async` feature + ring
    /// successfully constructed). Observable for callers who want
    /// to confirm the fast path is engaged.
    ///
    /// On non-Linux builds and on Linux without the `async`
    /// feature, this always returns `false`.
    #[must_use]
    pub fn native_iouring_active(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "async"))]
        {
            matches!(self.native_ring.get(), Some(Some(_)))
        }
        #[cfg(not(all(target_os = "linux", feature = "async")))]
        {
            false
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Linux + async — native io_uring substrate paths
// ─────────────────────────────────────────────────────────────────

#[cfg(all(target_os = "linux", feature = "async"))]
impl JournalHandle {
    /// Lazily constructs (or fetches the cached) io_uring substrate
    /// for this journal. Returns `None` if construction failed
    /// (kernel without io_uring, container restriction, etc.) —
    /// callers fall back to `spawn_blocking`.
    fn native_ring(&self) -> Option<&AsyncIoUring> {
        let outer = self.native_ring.get_or_init(|| {
            // Construct inside a tokio runtime context — the
            // caller has already verified `require_runtime()`.
            AsyncIoUring::new(JOURNAL_IOURING_DEPTH).ok().map(Arc::new)
        });
        outer.as_ref().map(|arc| arc.as_ref())
    }

    /// Native append — encode the framed record and submit
    /// `IORING_OP_WRITE` SQE at the reserved offset.
    ///
    /// 0.9.6 audit fix: takes `&self` rather than `self: Arc<Self>`.
    /// The caller holds a `&self` borrow via `self.native_ring()`
    /// which returns `Option<&AsyncIoUring>` tied to that borrow;
    /// the pre-0.9.6 `Arc<Self>` signature forced the caller to
    /// move `self` while `ring` was still borrowed, surfacing as
    /// `error[E0505]: cannot move out of self because it is
    /// borrowed` on the `--no-default-features --features async`
    /// build (caught by the new feature-matrix CI job, not the
    /// default-features Linux test).
    ///
    /// 1.1.1: takes `&Arc<Self>` so the op can hold a clone of the
    /// journal as its file keep-alive, and moves the frame into the
    /// driver. Once the first poll has queued the write, dropping
    /// this future cannot free the frame or close the fd while the
    /// kernel is still writing, and the reserved LSN range is always
    /// filled.
    ///
    /// 1.1.1: follows the sync append's protocol. A poisoned journal
    /// refuses the append; the write registers with the write gate
    /// before the `SeqCst` reservation, so a group-commit leader
    /// (sync or async) cannot publish a durable frontier over it
    /// before it is written; and a failed or short write poisons the
    /// journal. The gate registration and the poisoning live in an
    /// [`AppendInFlight`] owned by the io_uring driver, so both
    /// happen when the kernel is done with the write, whether or not
    /// this future is still being polled.
    async fn append_native(self: &Arc<Self>, ring: &AsyncIoUring, record: Vec<u8>) -> Result<Lsn> {
        use std::os::fd::AsRawFd;

        // A poisoned journal accepts nothing: an earlier failure may
        // have left a hole that this record would sit behind.
        self.poison.check()?;

        // Encode before reserving so an encode or size error never
        // leaves a hole in the LSN space. The CRC computation is
        // ~500 ns at 4 KiB on modern x86, far less than the
        // kernel-side write latency, so no win pushing it to the
        // io_uring side.
        let frame = crate::journal::format::encode_frame_owned(&record)?;
        let frame_bytes = frame.len();
        let frame_len = frame_bytes as u64;

        // Register with the write gate before reserving. The gate is
        // closed only while `preallocate` restores the file length;
        // yield instead of blocking the worker thread meanwhile.
        let ticket = loop {
            if let Some(ticket) = self.write_gate.try_enter() {
                break ticket;
            }
            tokio::task::yield_now().await;
        };
        // `SeqCst`, as in the sync append: the gate's drain argument
        // (see `journal/gate.rs`) relies on the single total order of
        // the epoch reads, this `fetch_add` and the leader's frontier
        // load.
        let start = self.next_lsn.fetch_add(frame_len, Ordering::SeqCst);
        let end = start + frame_len;
        // No `.await` between the registration and handing it to the
        // driver-owned op below, so cancellation cannot strand it.
        let in_flight = Arc::new(AppendInFlight {
            journal: Arc::clone(self),
            ticket: Mutex::new(Some(ticket.detach())),
            frame_bytes,
            written: AtomicBool::new(false),
        });
        let file = FileRef::with_hook(in_flight, |op| op.journal.file.as_raw_fd());
        let n = crate::async_io::iouring_substrate::write_at_native(
            ring,
            file,
            IoBuf::Vec(frame),
            start,
        )
        .await?;
        if n != frame_bytes {
            // `AppendInFlight` has already poisoned the journal.
            return Err(Error::Io(std::io::Error::other(
                "native io_uring write returned short count on journal append",
            )));
        }
        Ok(Lsn::new(end))
    }

    /// Native group-commit fsync — submit `IORING_OP_FSYNC(DATASYNC)`
    /// SQE and update synced_lsn after completion.
    ///
    /// 0.9.1: ports the sync path's leader/follower coordinator
    /// to the async substrate. The state mutex is acquired non-
    /// blocking via `try_lock`; on contention we yield the
    /// tokio worker rather than parking on a Condvar (which
    /// would block the worker thread). The async leader skips
    /// the `group_commit_window` follower-batching wait — async
    /// callers naturally arrive on a different timescale than
    /// sync callers, and the io_uring fsync is itself zero-
    /// syscall-cost on the submitter side.
    ///
    /// 1.1.1: the state lock is taken only inside the synchronous
    /// [`Self::try_lead_group_commit`] helper and in
    /// [`LeaderLease::end`], so no `parking_lot` guard is ever alive
    /// across an `.await` and the future stays `Send` (it was `!Send`
    /// on Linux in 1.1.0 because the guard binding spanned the
    /// yields). The leader role is held by a [`LeaderLease`], so
    /// dropping this future still clears `in_flight`; in 1.1.0 a
    /// cancelled leader left it set and every later sync on the
    /// journal waited forever.
    ///
    /// 1.1.1: follows the sync leader's protocol. A caller whose
    /// target is not yet durable gets the poison error; the leader
    /// drains the write gate before it captures the frontier, checks
    /// the poison flag again before the fsync, and a failed fsync
    /// poisons the journal. Once the fsync is submitted the lease
    /// belongs to the io_uring driver, which ends it when the kernel
    /// reports the result: a cancelled leader's fsync can neither
    /// overlap the next leader's fsync nor fail unnoticed.
    async fn sync_through_native(self: &Arc<Self>, ring: &AsyncIoUring, lsn: Lsn) -> Result<()> {
        use std::os::fd::AsRawFd;

        let lsn_off = lsn.as_u64();
        loop {
            // Atomic-load fast path: cheaper than a lock acquire
            // when the durable frontier already covers our target.
            // As in the sync path, a target that became durable
            // before a later failure stays durable, so this path
            // ignores the poison flag.
            if self.synced_lsn.load(Ordering::Acquire) >= lsn_off {
                return Ok(());
            }
            match self.try_lead_group_commit(lsn_off)? {
                LeaderAttempt::Covered => return Ok(()),
                // Lock contended, or another caller (sync or async)
                // is running the fsync. Yield; on resume the
                // synced_lsn fast path or committed_lsn re-check
                // will likely cover us.
                LeaderAttempt::Busy => {
                    tokio::task::yield_now().await;
                    continue;
                }
                LeaderAttempt::Leader(mut lease) => {
                    // Dropping `lease` on any early return (or on
                    // cancellation while draining) clears `in_flight`
                    // without publishing.
                    lease.frontier = self.drain_native_writes().await?;
                    // A write that failed after this leader was
                    // elected left a hole below the frontier; do not
                    // fsync and publish past it.
                    self.poison.check()?;
                    let file =
                        FileRef::with_hook(Arc::new(lease), |lease| lease.journal.file.as_raw_fd());
                    // The driver ends the lease when the fsync
                    // completes: it publishes the frontier on
                    // success and poisons the journal on failure.
                    return crate::async_io::iouring_substrate::fdatasync_native(ring, file).await;
                }
            }
        }
    }

    /// Waits until every buffered write reserved below the current
    /// reservation frontier has been written, and returns that
    /// frontier: the async counterpart of the gate drain in the sync
    /// leader.
    ///
    /// The lock-free `WriteGate::try_quiescent` check is retried a
    /// few times with a yield in between (in-flight io_uring writes
    /// complete on the driver task, which may need this worker).
    /// Under sustained append load it can keep failing, so the
    /// blocking, starvation-free `WriteGate::drain_below` then runs
    /// on the blocking pool rather than on a runtime worker.
    async fn drain_native_writes(self: &Arc<Self>) -> Result<u64> {
        for _ in 0..QUIESCENT_ATTEMPTS {
            let frontier = self
                .write_gate
                .try_quiescent(|| self.next_lsn.load(Ordering::SeqCst));
            if let Some(frontier) = frontier {
                return Ok(frontier);
            }
            tokio::task::yield_now().await;
        }
        let journal = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            journal
                .write_gate
                .drain_below(|| journal.next_lsn.load(Ordering::SeqCst))
        })
        .await
        .map_err(join_error_to_io)
    }

    /// Tries to become the group-commit leader for `lsn_off` without
    /// blocking. On [`LeaderAttempt::Leader`] this caller has set
    /// `in_flight`; the returned lease clears it when it ends.
    ///
    /// # Errors
    ///
    /// The poison error when the target is not yet durable and an
    /// earlier write, flush or fsync failed. As in the sync path, a
    /// failed leader poisons the journal before it clears
    /// `in_flight`, so a caller waiting behind it sees the failure
    /// here instead of retrying the fsync itself.
    fn try_lead_group_commit(self: &Arc<Self>, lsn_off: u64) -> Result<LeaderAttempt> {
        // Non-blocking try_lock so the tokio worker isn't parked on a
        // contended mutex.
        let Some(mut state) = self.group_commit.state.try_lock() else {
            return Ok(LeaderAttempt::Busy);
        };
        if state.committed_lsn >= lsn_off {
            return Ok(LeaderAttempt::Covered);
        }
        self.poison.check()?;
        if state.in_flight {
            return Ok(LeaderAttempt::Busy);
        }
        // Become leader. The lock is released on return, before the
        // SQE is submitted, so concurrent followers observe the
        // in-flight state.
        state.in_flight = true;
        Ok(LeaderAttempt::Leader(LeaderLease {
            journal: Arc::clone(self),
            frontier: 0,
            synced: AtomicBool::new(false),
        }))
    }
}

/// How many times the native leader retries the lock-free gate check
/// before it falls back to a blocking drain on the blocking pool.
#[cfg(all(target_os = "linux", feature = "async"))]
const QUIESCENT_ATTEMPTS: u32 = 16;

/// Outcome of [`JournalHandle::try_lead_group_commit`].
#[cfg(all(target_os = "linux", feature = "async"))]
enum LeaderAttempt {
    /// A completed fsync already covers the target LSN.
    Covered,
    /// The state lock is contended or another fsync is in flight.
    Busy,
    /// This caller set `in_flight` and runs the fsync.
    Leader(LeaderLease),
}

/// The group-commit leader role of a native async leader.
///
/// Held by the leader's future until the fsync is submitted, then
/// owned by the io_uring driver (as the fsync's [`OpHook`]) until
/// the kernel reports the result. Ending it publishes `frontier`
/// when the fsync succeeded, clears `in_flight` and wakes parked
/// sync-path followers. Because that happens in `Drop` (or in
/// [`OpHook::abandoned`]), it also runs when the leader's future is
/// cancelled; the next caller then becomes leader and issues its own
/// fsync.
#[cfg(all(target_os = "linux", feature = "async"))]
struct LeaderLease {
    journal: Arc<JournalHandle>,
    /// Frontier the gate drain proved written; published if the
    /// fsync succeeds.
    frontier: u64,
    /// Set by [`OpHook::finished`] on a successful fsync.
    synced: AtomicBool,
}

#[cfg(all(target_os = "linux", feature = "async"))]
impl LeaderLease {
    /// Ends the leader role. Runs exactly once: from `Drop`, or from
    /// [`OpHook::abandoned`], after which the driver leaks the lease
    /// without dropping it.
    fn end(&self) {
        let journal = &self.journal;
        let mut state = journal.group_commit.state.lock();
        if self.synced.load(Ordering::Acquire) && self.frontier > state.committed_lsn {
            state.committed_lsn = self.frontier;
            journal.synced_lsn.store(self.frontier, Ordering::Release);
        }
        state.in_flight = false;
        let _woken = journal.group_commit.cv_followers.notify_all();
    }
}

#[cfg(all(target_os = "linux", feature = "async"))]
impl Drop for LeaderLease {
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(all(target_os = "linux", feature = "async"))]
impl OpHook for LeaderLease {
    fn finished(&self, result: &std::io::Result<usize>) {
        match result {
            Ok(_) => self.synced.store(true, Ordering::Release),
            // fsyncgate: after a failed fsync the kernel may have
            // dropped the dirty pages, and a retry can report success
            // without writing them. Poison (before `end` clears
            // `in_flight`) instead of letting a later leader retry.
            Err(e) => self.journal.poison.set(&copy_io_error(e)),
        }
    }

    fn abandoned(&self) {
        self.journal.poison.set(&Error::Io(std::io::Error::other(
            "the io_uring driver stopped with a journal fsync in flight",
        )));
        self.end();
    }
}

/// One native async append between its LSN reservation and the end
/// of its write. Owned by the io_uring driver as the write's file
/// keep-alive and [`OpHook`].
///
/// Ending it returns the write-gate registration, so a leader's
/// drain waits exactly until the kernel is done with the write. A
/// write that did not complete in full (failed, short, never
/// submitted, or abandoned by a stopped driver) left a hole at an
/// LSN range later appends may already have been handed past, so the
/// journal is poisoned first, as `write_reserved` does for the sync
/// append.
#[cfg(all(target_os = "linux", feature = "async"))]
struct AppendInFlight {
    journal: Arc<JournalHandle>,
    /// The gate registration; `None` once returned. A lock rather
    /// than a plain field because `OpHook::abandoned` gets `&self`.
    /// It is never contended.
    ticket: Mutex<Option<DetachedTicket>>,
    frame_bytes: usize,
    /// Set by [`OpHook::finished`] when the whole frame was written.
    written: AtomicBool,
}

#[cfg(all(target_os = "linux", feature = "async"))]
impl AppendInFlight {
    /// Poisons the journal unless the write completed, then returns
    /// the gate registration. The poison is set before the
    /// registration ends, so a leader whose drain waited for this
    /// write sees it.
    fn end(&self, ticket: DetachedTicket) {
        if !self.written.load(Ordering::Acquire) {
            self.journal.poison.set(&Error::Io(std::io::Error::other(
                "async journal append did not finish its write",
            )));
        }
        self.journal.write_gate.leave_detached(ticket);
    }
}

#[cfg(all(target_os = "linux", feature = "async"))]
impl Drop for AppendInFlight {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.get_mut().take() {
            self.end(ticket);
        }
    }
}

#[cfg(all(target_os = "linux", feature = "async"))]
impl OpHook for AppendInFlight {
    fn finished(&self, result: &std::io::Result<usize>) {
        match result {
            Ok(n) if *n == self.frame_bytes => self.written.store(true, Ordering::Release),
            Ok(_) => self.journal.poison.set(&Error::Io(std::io::Error::other(
                "native io_uring write returned short count on journal append",
            ))),
            Err(e) => self.journal.poison.set(&copy_io_error(e)),
        }
    }

    fn abandoned(&self) {
        // The driver leaks this value right after the call, so `Drop`
        // never runs; end the registration here. The write may still
        // land, but nothing can confirm it, so `written` stays false
        // and the journal is poisoned.
        let ticket = self.ticket.lock().take();
        if let Some(ticket) = ticket {
            self.end(ticket);
        }
    }
}

/// Copies a kernel error reported to a hook (which only borrows it)
/// into the crate error the poison flag records.
#[cfg(all(target_os = "linux", feature = "async"))]
fn copy_io_error(e: &std::io::Error) -> Error {
    Error::Io(match e.raw_os_error() {
        Some(code) => std::io::Error::from_raw_os_error(code),
        None => std::io::Error::new(e.kind(), e.to_string()),
    })
}

fn join_error_to_io(e: tokio::task::JoinError) -> Error {
    Error::Io(std::io::Error::other(format!(
        "spawn_blocking task failed: {e}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static C: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(tag: &str) -> PathBuf {
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_journal_async_test_{}_{}_{tag}",
            std::process::id(),
            n
        ))
    }

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// 0.9.6 hardening: wraps an async test body with a hard
    /// 15-second timeout so a regression hangs in seconds, not
    /// the GitHub Actions default 6-hour job timeout.
    async fn with_timeout<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        const TIMEOUT_SECS: u64 = 15;
        match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), fut).await {
            Ok(v) => v,
            Err(_) => panic!(
                "test exceeded {TIMEOUT_SECS}s timeout — likely a hang in the async journal path"
            ),
        }
    }

    #[tokio::test]
    async fn append_async_returns_lsn_advanced_by_framed_len() {
        with_timeout(async {
            // Each record is framed (12 bytes overhead). LSN
            // advances by payload + 12.
            let path = tmp_path("append_async");
            let _g = Cleanup(path.clone());
            let fs = builder().build().expect("handle");
            let log = Arc::new(fs.journal(&path).expect("journal"));

            let lsn1 = log
                .clone()
                .append_async(b"hello".to_vec())
                .await
                .expect("a1");
            assert_eq!(lsn1, Lsn::new(5 + 12));

            let lsn2 = log
                .clone()
                .append_async(b" world".to_vec())
                .await
                .expect("a2");
            assert_eq!(lsn2, Lsn::new(17 + 6 + 12));
        })
        .await;
    }

    #[tokio::test]
    async fn sync_through_async_advances_synced_lsn() {
        with_timeout(async {
            let path = tmp_path("sync_through_async");
            let _g = Cleanup(path.clone());
            let fs = builder().build().expect("handle");
            let log = Arc::new(fs.journal(&path).expect("journal"));

            let lsn = log
                .clone()
                .append_async(b"durable".to_vec())
                .await
                .expect("append");
            log.clone().sync_through_async(lsn).await.expect("sync");
            assert!(log.synced_lsn() >= lsn);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_async_appends_all_succeed() {
        with_timeout(async {
            let path = tmp_path("concurrent_async");
            let _g = Cleanup(path.clone());
            let fs = builder().build().expect("handle");
            let log = Arc::new(fs.journal(&path).expect("journal"));

            let mut joins = Vec::new();
            for i in 0..32 {
                let log = log.clone();
                let payload = format!("rec {i:04}").into_bytes();
                joins.push(tokio::spawn(async move { log.append_async(payload).await }));
            }
            let mut max_lsn = Lsn::ZERO;
            for j in joins {
                let lsn = j.await.expect("join").expect("append_async");
                if lsn > max_lsn {
                    max_lsn = lsn;
                }
            }
            log.clone()
                .sync_through_async(max_lsn)
                .await
                .expect("final sync");
            assert!(log.synced_lsn() >= max_lsn);
        })
        .await;
    }

    #[tokio::test]
    async fn direct_mode_async_round_trip() {
        with_timeout(async {
            let path = tmp_path("direct_async");
            let _g = Cleanup(path.clone());
            let fs = builder().build().expect("handle");
            let log = Arc::new(
                fs.journal_with(&path, crate::JournalOptions::new().direct(true))
                    .expect("direct journal"),
            );

            // Async append then async sync — direct-mode journals
            // route both through spawn_blocking so the buffer mutex
            // is honoured.
            let lsn = log
                .clone()
                .append_async(b"async direct payload".to_vec())
                .await
                .expect("append_async");
            log.clone()
                .sync_through_async(lsn)
                .await
                .expect("sync_through_async");
            assert!(log.synced_lsn() >= lsn);

            // Native io_uring is NOT engaged for direct-mode journals
            // (we fall back to spawn_blocking).
            // On non-direct journals this would be `true`; here it
            // must be `false` because we never construct the ring.
            assert!(!log.native_iouring_active());
        })
        .await;
    }

    /// FS-H5: both async journal futures must be `Send` so callers
    /// can hand them to `tokio::spawn`. In 1.1.0 the Linux native
    /// path kept a `parking_lot` guard binding alive across `.await`,
    /// which made `sync_through_async` `!Send` there; this test then
    /// failed to compile on Linux.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_async_journal_futures_are_send() {
        fn assert_send<T: Send>(_: &T) {}
        with_timeout(async {
            let path = tmp_path("send");
            let _g = Cleanup(path.clone());
            let fs = builder().build().expect("handle");
            let log = Arc::new(fs.journal(&path).expect("journal"));

            let append = log.clone().append_async(b"spawned".to_vec());
            assert_send(&append);
            let lsn = tokio::spawn(append).await.expect("join").expect("append");

            let sync = log.clone().sync_through_async(lsn);
            assert_send(&sync);
            tokio::spawn(sync).await.expect("join").expect("sync");
            assert!(log.synced_lsn() >= lsn);
        })
        .await;
    }

    /// FS-H1: dropping a `sync_through_async` leader after its first
    /// poll (fsync submitted, `in_flight` set) must not wedge the
    /// journal. In 1.1.0 `in_flight` stayed `true` and every later
    /// sync, async or blocking, waited forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cancelled_sync_leader_does_not_block_later_syncs() {
        with_timeout(async {
            let path = tmp_path("cancel_leader");
            let _g = Cleanup(path.clone());
            let fs = builder().build().expect("handle");
            let log = Arc::new(fs.journal(&path).expect("journal"));

            for round in 0..8u8 {
                let lsn = log
                    .clone()
                    .append_async(vec![round; 128])
                    .await
                    .expect("append");
                {
                    let fut = log.clone().sync_through_async(lsn);
                    tokio::pin!(fut);
                    // Poll the leader exactly once, then drop it.
                    tokio::select! {
                        biased;
                        r = &mut fut => r.expect("leader finished on first poll"),
                        () = std::future::ready(()) => {}
                    }
                }

                let next = log
                    .clone()
                    .append_async(vec![round; 64])
                    .await
                    .expect("append after cancel");
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    log.clone().sync_through_async(next),
                )
                .await
                .expect("sync_through_async hung after a cancelled leader")
                .expect("sync_through_async");

                let blocking = log.clone();
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tokio::task::spawn_blocking(move || blocking.sync_through(next)),
                )
                .await
                .expect("sync_through hung after a cancelled leader")
                .expect("join")
                .expect("sync_through");
                assert!(log.synced_lsn() >= next);
            }
        })
        .await;
    }

    /// Opens a buffered journal with no group-commit window and
    /// settles its async substrate with one durable append. Returns
    /// `None` when the native io_uring path is not active (kernel or
    /// sandbox without io_uring), so the caller skips.
    #[cfg(target_os = "linux")]
    async fn native_journal(path: &std::path::Path) -> Option<(Arc<JournalHandle>, Lsn)> {
        let fs = builder().build().expect("handle");
        let log = Arc::new(
            fs.journal_with(path, crate::JournalOptions::new().group_commit_window(None))
                .expect("journal"),
        );
        let first = log
            .clone()
            .append_async(b"settle".to_vec())
            .await
            .expect("append");
        log.clone().sync_through_async(first).await.expect("sync");
        log.native_iouring_active().then_some((log, first))
    }

    /// Polls `fut` exactly once, without yielding to the scheduler,
    /// then drops it. (`tokio::time::timeout(Duration::ZERO, ..)` is
    /// not a substitute: its timer can report `Pending` and let other
    /// tasks run before it fires.)
    ///
    /// Returns `true` when the op was still in flight at the drop. On a
    /// fast kernel the write can complete on the first poll; the
    /// invariants the callers check (later syncs complete, the durable
    /// frontier covers only written bytes) must hold either way, so the
    /// callers do not fail on `false`.
    #[cfg(target_os = "linux")]
    async fn poll_once_then_drop<F: std::future::Future>(fut: F) -> bool {
        let mut fut = std::pin::pin!(fut);
        let polled = std::future::poll_fn(|cx| std::task::Poll::Ready(fut.as_mut().poll(cx))).await;
        polled.is_pending()
    }

    /// End LSN of the longest prefix of cleanly decoded records.
    #[cfg(target_os = "linux")]
    fn clean_prefix_end(path: &std::path::Path) -> u64 {
        let mut reader = crate::JournalReader::open(path).expect("reader");
        let mut end = 0u64;
        for record in reader.iter() {
            match record {
                Ok(r) => {
                    end = r.lsn.as_u64() + r.payload.len() as u64 + 12;
                }
                Err(_) => break,
            }
        }
        end
    }

    /// An async append registers with the journal's write gate
    /// before it reserves its LSN range, so a blocking
    /// `sync_through` leader cannot publish a frontier over an
    /// append whose write is still queued. The runtime is
    /// single-threaded and this task does not yield while the leader
    /// runs, so the io_uring owner task cannot start the write.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn test_blocking_leader_waits_for_queued_async_append() {
        with_timeout(async {
            let path = tmp_path("gate_blocking_leader");
            let _g = Cleanup(path.clone());
            let Some((log, first)) = native_journal(&path).await else {
                return;
            };

            // Poll once (reserves and queues the write), then drop
            // the future.
            let fut = log.clone().append_async(vec![0xA7; 256 * 1024]);
            let _ = poll_once_then_drop(fut).await;
            let target = log.next_lsn();
            assert!(target > first);

            let leader = {
                let log = Arc::clone(&log);
                std::thread::spawn(move || log.sync_through(target))
            };
            // Deliberately block this runtime's only thread.
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert!(
                log.synced_lsn() < target,
                "leader published a frontier over a queued, unwritten append"
            );
            assert!(!leader.is_finished(), "leader did not wait for the write");

            // Let the owner task run the write; the leader then
            // drains, fsyncs and publishes.
            while !leader.is_finished() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            leader.join().expect("leader").expect("sync_through");
            assert!(log.synced_lsn() >= target);
            assert!(clean_prefix_end(&path) >= log.synced_lsn().as_u64());
        })
        .await;
    }

    /// Cancelling an async append mid-flight must not leave its
    /// write-gate registration behind: the driver ends it when the
    /// write completes, so later leaders (async and blocking) finish,
    /// and the published frontier only covers written bytes.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cancelled_append_mid_flight_then_sync_completes() {
        with_timeout(async {
            let path = tmp_path("cancel_append_gate");
            let _g = Cleanup(path.clone());
            let Some((log, _first)) = native_journal(&path).await else {
                return;
            };
            for round in 0..4u8 {
                let fut = log.clone().append_async(vec![round + 1; 8 * 1024 * 1024]);
                let _ = poll_once_then_drop(fut).await;
                let target = log.next_lsn();

                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    log.clone().sync_through_async(target),
                )
                .await
                .expect("sync_through_async hung after a cancelled append")
                .expect("sync_through_async");
                assert!(log.synced_lsn() >= target);
                assert!(clean_prefix_end(&path) >= log.synced_lsn().as_u64());

                let fut = log.clone().append_async(vec![round + 1; 8 * 1024 * 1024]);
                let _ = poll_once_then_drop(fut).await;
                let target = log.next_lsn();
                let blocking = Arc::clone(&log);
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tokio::task::spawn_blocking(move || blocking.sync_through(target)),
                )
                .await
                .expect("sync_through hung after a cancelled append")
                .expect("join")
                .expect("sync_through");
                assert!(log.synced_lsn() >= target);
                assert!(clean_prefix_end(&path) >= log.synced_lsn().as_u64());
            }
        })
        .await;
    }

    /// On a single-threaded runtime the async leader must not block
    /// its worker while it waits for a cancelled append's write: the
    /// io_uring owner task needs that worker to complete the write.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn test_async_leader_does_not_block_current_thread_runtime() {
        with_timeout(async {
            let path = tmp_path("leader_current_thread");
            let _g = Cleanup(path.clone());
            let Some((log, _first)) = native_journal(&path).await else {
                return;
            };
            let fut = log.clone().append_async(vec![0x3C; 4 * 1024 * 1024]);
            let _ = poll_once_then_drop(fut).await;
            let target = log.next_lsn();
            log.clone()
                .sync_through_async(target)
                .await
                .expect("sync_through_async");
            assert!(log.synced_lsn() >= target);
            assert_eq!(clean_prefix_end(&path), target.as_u64());
        })
        .await;
    }

    /// Swaps the journal's file handle. Every driver-owned clone of
    /// the journal is released before an awaited op returns, so the
    /// test holds the only reference here.
    #[cfg(target_os = "linux")]
    fn swap_file(log: &mut Arc<JournalHandle>, file: std::fs::File) {
        Arc::get_mut(log).expect("unique journal").file = file;
    }

    #[cfg(target_os = "linux")]
    fn writable(path: &std::path::Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("writable handle")
    }

    #[cfg(target_os = "linux")]
    fn assert_poisoned<T: std::fmt::Debug>(r: Result<T>) {
        match r {
            Err(Error::Io(e)) => assert!(
                e.to_string().contains("poisoned"),
                "expected the poison error, got {e}"
            ),
            other => panic!("expected the poison error, got {other:?}"),
        }
    }

    /// A failed native write poisons the journal like a failed sync
    /// append: later async (and sync) appends and syncs of targets
    /// that are not yet durable fail, even with a healthy handle
    /// back.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_native_append_write_failure_poisons_journal() {
        with_timeout(async {
            let path = tmp_path("native_poison_write");
            let _g = Cleanup(path.clone());
            let Some((mut log, durable)) = native_journal(&path).await else {
                return;
            };
            let acked = log
                .clone()
                .append_async(b"acked-not-synced".to_vec())
                .await
                .expect("append");

            swap_file(&mut log, std::fs::File::open(&path).expect("read-only"));
            let failed = log.clone().append_async(b"fails".to_vec()).await;
            assert!(failed.is_err(), "write through a read-only handle");

            swap_file(&mut log, writable(&path));
            assert_poisoned(log.clone().append_async(b"later".to_vec()).await);
            assert_poisoned(log.clone().sync_through_async(acked).await);
            assert_poisoned(log.append(b"sync later"));
            // A target that was durable before the failure is still
            // reported durable.
            log.clone()
                .sync_through_async(durable)
                .await
                .expect("already durable");
        })
        .await;
    }

    /// The write fails after its future was dropped: the driver still
    /// reports the result, and the journal is poisoned before the
    /// gate registration ends, so the next leader refuses to publish.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_cancelled_failing_append_still_poisons_journal() {
        with_timeout(async {
            let path = tmp_path("native_poison_cancelled");
            let _g = Cleanup(path.clone());
            let Some((mut log, _first)) = native_journal(&path).await else {
                return;
            };
            swap_file(&mut log, std::fs::File::open(&path).expect("read-only"));
            let fut = log
                .clone()
                .append_async(b"fails in the background".to_vec());
            let _ = poll_once_then_drop(fut).await;
            let target = log.next_lsn();
            assert_poisoned(log.clone().sync_through_async(target).await);
            assert!(log.synced_lsn() < target, "a poisoned sync published");
        })
        .await;
    }

    /// fsyncgate on the native path: a failed `IORING_OP_FSYNC`
    /// poisons the journal and is not retried by the next caller.
    /// `/dev/null` rejects fsync with `EINVAL`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_native_sync_failure_poisons_and_is_not_retried() {
        with_timeout(async {
            let path = tmp_path("native_poison_fsync");
            let _g = Cleanup(path.clone());
            let Some((mut log, _first)) = native_journal(&path).await else {
                return;
            };
            let lsn = log
                .clone()
                .append_async(b"record".to_vec())
                .await
                .expect("append");
            let dev_null = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .expect("open /dev/null");
            swap_file(&mut log, dev_null);
            let failed = log.clone().sync_through_async(lsn).await;
            assert!(
                matches!(&failed, Err(Error::Io(e)) if !e.to_string().contains("poisoned")),
                "expected the fsync error itself, got {failed:?}"
            );

            swap_file(&mut log, writable(&path));
            assert_poisoned(log.clone().sync_through_async(lsn).await);
            assert_poisoned(log.sync_through(lsn));
            assert_poisoned(log.clone().append_async(b"after".to_vec()).await);
            assert!(log.synced_lsn() < lsn, "failed sync must not publish");
        })
        .await;
    }

    /// An append whose op is dropped before the driver ever ran it
    /// (driver gone, op still queued) did not write its reserved
    /// range: dropping its in-flight record poisons the journal and
    /// still releases the gate, so a leader does not wait forever.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_unsubmitted_append_poisons_and_releases_gate() {
        with_timeout(async {
            let path = tmp_path("native_unsubmitted");
            let _g = Cleanup(path.clone());
            let fs = builder().build().expect("handle");
            let log = Arc::new(fs.journal(&path).expect("journal"));
            let ticket = log.write_gate.enter().detach();
            let in_flight = AppendInFlight {
                journal: Arc::clone(&log),
                ticket: Mutex::new(Some(ticket)),
                frame_bytes: 64,
                written: AtomicBool::new(false),
            };
            drop(in_flight);
            assert_poisoned(log.append(b"after"));
            let drained = {
                let log = Arc::clone(&log);
                tokio::task::spawn_blocking(move || log.write_gate.drain_below(|| 7))
            };
            let frontier = tokio::time::timeout(std::time::Duration::from_secs(5), drained)
                .await
                .expect("drain waited for a dropped registration")
                .expect("join");
            assert_eq!(frontier, 7);
        })
        .await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn native_iouring_engages_on_linux_when_available() {
        with_timeout(async {
            let path = tmp_path("native_engage");
            let _g = Cleanup(path.clone());
            let fs = builder().build().expect("handle");
            let log = Arc::new(fs.journal(&path).expect("journal"));

            // Trigger lazy construction by doing one append.
            let _ = log
                .clone()
                .append_async(b"trigger".to_vec())
                .await
                .expect("append");

            // The first append settles the substrate choice: the
            // OnceLock is populated either way, and the native path
            // is active exactly when this kernel lets us build an
            // async ring (sandboxed CI without io_uring falls back).
            assert!(
                log.native_ring.get().is_some(),
                "first append_async must settle the substrate choice"
            );
            let ring_available = AsyncIoUring::new(8).is_ok();
            let active = log.native_iouring_active();
            assert_eq!(active, ring_available);
            assert_eq!(
                log.backend_kind() == crate::JournalBackendKind::KernelIoUring,
                active
            );
        })
        .await;
    }
}
