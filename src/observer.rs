//! [`FsysObserver`] — opt-in structured telemetry for fsys hot paths.
//!
//! 0.9.2 introduces the observer trait so that downstream consumers
//! (databases, embedded storage engines, monitoring layers) can
//! plug latency / throughput collection into fsys without paying any
//! cost when no observer is registered.
//!
//! ## Design
//!
//! - The trait is defined as a single object-safe interface; callers
//!   register an `Arc<dyn FsysObserver>` via
//!   [`crate::Builder::observer`] at handle-construction time.
//! - All trait methods carry default no-op implementations, so a
//!   caller that only cares about journal sync events implements
//!   only [`FsysObserver::on_journal_sync`] and leaves the rest at
//!   their defaults.
//! - The instrumentation points hold a single
//!   `Option<Arc<dyn FsysObserver>>` field on each handle.
//!   When `None`, the cost per op is one branch — comparable to a
//!   `cfg!(feature = "tracing")` gate. When `Some`, the trait
//!   method dispatches once per op.
//! - Event types are `#[non_exhaustive]` so the library can extend
//!   them in patch releases without breaking observer
//!   implementations.
//! - Observers run on the calling thread of the instrumented op.
//!   They MUST NOT block, panic, or perform IO that could deadlock
//!   on the same handle. The library does not catch panics inside
//!   observer callbacks; an observer panic propagates exactly like
//!   any other panic on the calling thread.
//!
//! ## What this is not
//!
//! - **Not a replacement for `tracing`.** The optional `tracing`
//!   feature stays — observers are for typed in-process telemetry
//!   (latency histograms, throughput counters), `tracing` is for
//!   distributed tracing. Both can be enabled simultaneously.
//! - **Not a hot-path log sink.** Observers must be cheap;
//!   high-frequency callers should accumulate into in-memory
//!   counters, not call back into a logger or external system on
//!   every event.
//!
//! ## Example: latency-histogram observer
//!
//! ```
//! use std::sync::atomic::{AtomicU64, Ordering};
//! use std::sync::Arc;
//! use std::time::Duration;
//! use fsys::observer::{FsysObserver, JournalSyncEvent};
//!
//! #[derive(Debug, Default)]
//! struct SyncLatency {
//!     count: AtomicU64,
//!     total_nanos: AtomicU64,
//! }
//!
//! impl FsysObserver for SyncLatency {
//!     fn on_journal_sync(&self, event: JournalSyncEvent) {
//!         self.count.fetch_add(1, Ordering::Relaxed);
//!         self.total_nanos
//!             .fetch_add(event.duration.as_nanos() as u64, Ordering::Relaxed);
//!     }
//! }
//!
//! let observer: Arc<dyn FsysObserver> = Arc::new(SyncLatency::default());
//! let fs = fsys::builder().observer(Arc::clone(&observer)).build().unwrap();
//! # let _ = fs;
//! # let _ = observer;
//! ```

use std::time::Duration;

/// Telemetry hook registered per-handle. See module docs for the full
/// design contract.
///
/// Every method has a default no-op body, so implementors override
/// only the events they care about. Implementors must be `Send +
/// Sync + Debug`. Methods MUST NOT block, panic, or invoke fsys IO
/// on the same handle.
pub trait FsysObserver: std::fmt::Debug + Send + Sync {
    /// Fired after a [`crate::JournalHandle::append`] or
    /// [`crate::JournalHandle::append_batch`] completes (success or
    /// failure). Called once per call regardless of how many records
    /// the call carried — see [`JournalAppendEvent::records`].
    fn on_journal_append(&self, _event: JournalAppendEvent) {}

    /// Fired after a [`crate::JournalHandle::sync_through`] completes
    /// (success or failure). For group-committed syncs, only the
    /// **leader** thread emits this event; followers return without
    /// firing — this is the design choice that makes per-syscall
    /// latency observable rather than per-caller latency.
    fn on_journal_sync(&self, _event: JournalSyncEvent) {}

    /// Fired after a [`crate::Handle::write`] (atomic-replace primitive)
    /// completes (success or failure).
    fn on_handle_write(&self, _event: HandleWriteEvent) {}

    /// Fired after a [`crate::Handle::read`] completes (success or
    /// failure).
    fn on_handle_read(&self, _event: HandleReadEvent) {}
}

/// Event payload for [`FsysObserver::on_journal_append`].
///
/// `#[non_exhaustive]` so future fields can be added without
/// breaking existing observer implementations.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct JournalAppendEvent {
    /// Total bytes written to the journal (record payload bytes
    /// plus per-record framing overhead).
    pub bytes_written: u64,
    /// Number of records the call submitted. `1` for
    /// [`crate::JournalHandle::append`], `N` for
    /// [`crate::JournalHandle::append_batch`].
    pub records: u32,
    /// Wall-time duration of the call as measured at the
    /// instrumentation site. Includes encoding, LSN reservation,
    /// and the underlying syscall(s); excludes time the caller
    /// spent constructing the payload.
    pub duration: Duration,
    /// `true` if the call returned an error.
    pub error: bool,
}

/// Event payload for [`FsysObserver::on_journal_sync`].
///
/// Only the leader's syscall fires this event; followers wake from
/// the leader's `notify_all` and return without emitting.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct JournalSyncEvent {
    /// LSN of the new durable frontier after this sync — i.e. the
    /// byte offset up to which the journal is now on stable
    /// storage. Equal to the previous `synced_lsn` on no-op syncs.
    pub durable_lsn: u64,
    /// Total wall-time duration of the call. For leaders, includes
    /// the optional `group_commit_window` wait, the actual fsync
    /// syscall, and the post-syscall state update.
    pub duration: Duration,
    /// Number of follower threads that joined this sync's batch by
    /// the time the leader exited its window. `0` means the leader
    /// fsynced alone.
    pub followers_at_commit: u32,
    /// `true` if the underlying fsync syscall failed.
    pub error: bool,
}

/// Event payload for [`FsysObserver::on_handle_write`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct HandleWriteEvent {
    /// Bytes written by the caller (payload size, not including
    /// any internal framing).
    pub bytes_written: u64,
    /// Wall-time duration of the call.
    pub duration: Duration,
    /// `true` if the call returned an error.
    pub error: bool,
}

/// Event payload for [`FsysObserver::on_handle_read`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct HandleReadEvent {
    /// Bytes returned to the caller.
    pub bytes_read: u64,
    /// Wall-time duration of the call.
    pub duration: Duration,
    /// `true` if the call returned an error.
    pub error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[derive(Debug, Default)]
    struct CountingObserver {
        appends: AtomicU64,
        syncs: AtomicU64,
        writes: AtomicU64,
        reads: AtomicU64,
    }

    impl FsysObserver for CountingObserver {
        fn on_journal_append(&self, _e: JournalAppendEvent) {
            let _ = self.appends.fetch_add(1, Ordering::Relaxed);
        }
        fn on_journal_sync(&self, _e: JournalSyncEvent) {
            let _ = self.syncs.fetch_add(1, Ordering::Relaxed);
        }
        fn on_handle_write(&self, _e: HandleWriteEvent) {
            let _ = self.writes.fetch_add(1, Ordering::Relaxed);
        }
        fn on_handle_read(&self, _e: HandleReadEvent) {
            let _ = self.reads.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn observer_default_methods_are_no_ops() {
        // A minimal observer that only overrides one method must
        // compile and the unimplemented methods must be safe to
        // call (default body is a no-op).
        #[derive(Debug)]
        struct OnlyAppend;
        impl FsysObserver for OnlyAppend {
            fn on_journal_append(&self, _e: JournalAppendEvent) {}
        }
        let obs: Arc<dyn FsysObserver> = Arc::new(OnlyAppend);
        obs.on_journal_sync(JournalSyncEvent {
            durable_lsn: 0,
            duration: Duration::ZERO,
            followers_at_commit: 0,
            error: false,
        });
        obs.on_handle_write(HandleWriteEvent {
            bytes_written: 0,
            duration: Duration::ZERO,
            error: false,
        });
        obs.on_handle_read(HandleReadEvent {
            bytes_read: 0,
            duration: Duration::ZERO,
            error: false,
        });
    }

    #[test]
    fn counting_observer_tallies_per_method() {
        let obs = Arc::new(CountingObserver::default());
        let dyn_obs: Arc<dyn FsysObserver> = obs.clone();
        for _ in 0..3 {
            dyn_obs.on_journal_append(JournalAppendEvent {
                bytes_written: 100,
                records: 1,
                duration: Duration::from_micros(10),
                error: false,
            });
        }
        dyn_obs.on_journal_sync(JournalSyncEvent {
            durable_lsn: 300,
            duration: Duration::from_micros(50),
            followers_at_commit: 0,
            error: false,
        });
        // Tally must reflect every dispatched event.
        assert_eq!(obs.appends.load(Ordering::Relaxed), 3);
        assert_eq!(obs.syncs.load(Ordering::Relaxed), 1);
        assert_eq!(obs.writes.load(Ordering::Relaxed), 0);
        assert_eq!(obs.reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn events_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JournalAppendEvent>();
        assert_send_sync::<JournalSyncEvent>();
        assert_send_sync::<HandleWriteEvent>();
        assert_send_sync::<HandleReadEvent>();
    }
}
