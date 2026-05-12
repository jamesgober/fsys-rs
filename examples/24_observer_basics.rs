//! # `FsysObserver` — structured telemetry hook
//!
//! 0.9.2 added an observer trait that downstream consumers can plug
//! into fsys's instrumented hot paths without paying any cost when
//! no observer is registered. Per-op cost when `None`: a single
//! `Option::is_some` branch.
//!
//! Four event hooks are declared on the trait; as of the current
//! release **only the journal-side hooks fire automatically**:
//!
//! - `on_journal_append` — after `JournalHandle::append` /
//!   `append_batch`. **Fires.**
//! - `on_journal_sync` — after `JournalHandle::sync_through`
//!   (leader only — followers don't fire). **Fires.**
//! - `on_handle_write` — after `Handle::write`.
//!   **Reserved — not currently instrumented.** Default no-op
//!   body; never invoked. Subscribe to your own per-write
//!   telemetry until the instrumentation site lands.
//! - `on_handle_read` — after `Handle::read`.
//!   **Reserved — not currently instrumented.** Same status.
//!
//! Each callback receives a typed event payload (`JournalAppendEvent`,
//! `JournalSyncEvent`, `HandleWriteEvent`, `HandleReadEvent`) with
//! bytes counts, durations, and error flags.
//!
//! ## Observer contract — what NOT to do inside a callback
//!
//! - **Don't block.** Callbacks run on the calling thread.
//! - **Don't panic.** The library doesn't `catch_unwind` around
//!   observer calls; a panic crashes the calling thread.
//! - **Don't invoke fsys methods on the same handle.** Deadlock /
//!   unbounded recursion risk. Methods on a *different* handle are
//!   safe.
//!
//! High-frequency callbacks should accumulate into atomics or
//! lock-free histograms rather than calling out to a logger /
//! external system on every event.
//!
//! ## When to use this pattern
//!
//! Production observability: Prometheus / OpenTelemetry exporters,
//! per-handle latency histograms, custom metrics for capacity
//! planning. Anything that needs typed in-process telemetry rather
//! than a tracing-style distributed-tracing surface.
//!
//! ## When NOT to use this pattern
//!
//! - Test code that's already using mocks or counters
//! - Distributed-tracing surfaces (use the `tracing` feature
//!   instead — it's lower-overhead for that shape)
//!
//! Run: `cargo run --example 24_observer_basics`

use fsys::observer::{
    FsysObserver, HandleReadEvent, HandleWriteEvent, JournalAppendEvent, JournalSyncEvent,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A minimal counting observer: tracks event counts + cumulative
/// nanoseconds for each event class. Production observers would
/// log to a histogram crate (e.g., `hdrhistogram`) instead of
/// flat sums.
#[derive(Debug, Default)]
struct CountingObserver {
    appends: AtomicU64,
    syncs: AtomicU64,
    writes: AtomicU64,
    reads: AtomicU64,
    total_sync_nanos: AtomicU64,
}

impl FsysObserver for CountingObserver {
    fn on_journal_append(&self, _: JournalAppendEvent) {
        self.appends.fetch_add(1, Ordering::Relaxed);
    }
    fn on_journal_sync(&self, event: JournalSyncEvent) {
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.total_sync_nanos
            .fetch_add(event.duration.as_nanos() as u64, Ordering::Relaxed);
    }
    fn on_handle_write(&self, _: HandleWriteEvent) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }
    fn on_handle_read(&self, _: HandleReadEvent) {
        self.reads.fetch_add(1, Ordering::Relaxed);
    }
}

fn main() -> fsys::Result<()> {
    let observer = Arc::new(CountingObserver::default());

    // Builder::observer takes Arc<dyn FsysObserver>. Pass a clone
    // so the observer concrete type stays accessible for read-out
    // at end of test.
    let dyn_obs: Arc<dyn FsysObserver> = observer.clone();
    let fs = Arc::new(fsys::builder().observer(dyn_obs).build()?);
    println!("handle built with observer registered");

    // Generate observable activity.
    let path = std::env::temp_dir().join("fsys_example_observer_basics");
    let _ = std::fs::remove_file(&path);

    // 5 writes.
    for i in 0..5 {
        fs.write(&path, format!("content {i}").as_bytes())?;
    }
    // 3 reads.
    for _ in 0..3 {
        let _ = fs.read(&path)?;
    }

    // Some journal activity.
    let journal_path = std::env::temp_dir().join("fsys_example_observer_journal.wal");
    let _ = std::fs::remove_file(&journal_path);
    let log = fs.journal(&journal_path)?;
    for i in 0..50 {
        log.append(format!("entry {i}").as_bytes())?;
    }
    log.sync_through(log.next_lsn())?;
    log.close()?;

    // Read the counters.
    println!();
    println!("observer counters after activity:");
    println!(
        "  journal appends:  {} (expected 50)",
        observer.appends.load(Ordering::Relaxed)
    );
    println!(
        "  journal syncs:    {} (expected 1)",
        observer.syncs.load(Ordering::Relaxed)
    );
    let total_sync = observer.total_sync_nanos.load(Ordering::Relaxed);
    let sync_count = observer.syncs.load(Ordering::Relaxed).max(1);
    println!("  avg sync latency: {} µs", total_sync / sync_count / 1000);
    println!();
    println!(
        "  handle writes:    {} (Handle::write is not currently",
        observer.writes.load(Ordering::Relaxed)
    );
    println!(
        "  handle reads:     {}  instrumented; the trait methods",
        observer.reads.load(Ordering::Relaxed)
    );
    println!("                     are reserved for a future release)");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&journal_path);
    Ok(())
}
