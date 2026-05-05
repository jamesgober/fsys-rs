//! Pipeline subsystem — solo and group lanes for IO dispatch.
//!
//! Every [`crate::Handle`] owns a `Pipeline` (this module's primary
//! type, kept `pub(crate)` because the pipeline is a crate-internal
//! concern; users interact with it indirectly via `Handle`'s batch
//! API). The pipeline implements the dual-lane model from `0.4.0`:
//!
//! - **Solo lane.** `Handle::write`, `read`, `append`, `write_at`, etc.
//!   call directly into `crud::file` / `crud::dir` functions (those
//!   modules are crate-internal). The pipeline is *not consulted*.
//!   Solo-lane ops have
//!   exactly the latency they had in `0.3.0` — the pipeline is invisible
//!   on this path.
//! - **Group lane.** `Handle::write_batch`, `delete_batch`, `copy_batch`,
//!   and `Batch::commit` route through this module. Ops are placed on a
//!   bounded MPMC queue and consumed by a per-handle dispatcher thread
//!   that accumulates batches under a hybrid time-or-count window
//!   (defaults: 1 ms / 128 ops / 1024-deep queue), executes them in
//!   strict submission order, and reports per-batch results back to the
//!   caller.
//!
//! The dispatcher thread is spawned **lazily** on the first batch
//! submission: idle handles cost zero threads. On `Pipeline` drop,
//! the dispatcher is signaled to drain pending jobs and exit; a 5-second
//! hard-timeout protects against a wedged dispatcher blocking
//! `Handle` drop indefinitely.
//!
//! ## Group-lane fence semantics by platform
//!
//! `0.4.0`'s dispatcher executes each batch op via the same
//! atomic-replace pattern as the solo lane — see decisions D-1 and
//! D-4(c) in `.dev/DECISIONS-0.4.0.md`. This means:
//!
//! | Platform | Per-op fence in the group lane |
//! |---|---|
//! | Linux | `fdatasync` (or `fsync` for `Method::Sync`) on the temp file before rename, then best-effort `fsync` on the parent directory after rename. |
//! | macOS | `fcntl(F_FULLFSYNC)` on the temp file before rename. |
//! | Windows | `FILE_FLAG_WRITE_THROUGH` makes each write durable; no separate fence syscall. The group-lane benefit on Windows comes from queue depth and syscall amortisation, not flush sharing. |
//!
//! ## Active method and group-lane fallbacks
//!
//! The dispatcher runs without a [`crate::Handle`] reference and therefore
//! cannot update [`crate::Handle::active_method`] when a per-op Direct IO
//! fallback occurs (e.g. one path in a batch lives on tmpfs and rejects
//! `O_DIRECT`). In `0.4.0`, that fallback information surfaces in the
//! `BatchError::source` of the failing op rather than aggregated to the
//! handle. This is decision D-5 in `.dev/DECISIONS-0.4.0.md`; full
//! cross-lane consistency arrives in `0.5.0` alongside the platform
//! module's IO state-machine consolidation.

use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::error::BatchError;

mod dispatch;
mod group;
mod solo;

use group::BatchJob;
pub(crate) use group::{BatchOp, HandleSnapshot};

/// Configuration knobs for the group-lane pipeline.
///
/// Set by the [`crate::Builder`] (`batch_window_ms`, `batch_size_max`,
/// `batch_queue_max`). Defaults match the prompt: 1 ms window, 128 ops
/// per batch, 1024-deep queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PipelineConfig {
    /// Maximum time the dispatcher waits for additional jobs after the
    /// first one arrives, before forcing a flush. Counted in
    /// milliseconds.
    pub batch_window_ms: u64,
    /// Maximum number of accumulated ops per batch flush. The
    /// dispatcher flushes when *either* the time window expires or this
    /// count is reached, whichever comes first.
    pub batch_size_max: usize,
    /// Bounded queue capacity. When the queue is full, callers
    /// submitting a batch *block* until space is available (decision #4
    /// — bounded queue with blocking submission).
    pub batch_queue_max: usize,
}

impl PipelineConfig {
    /// Default configuration. 1 ms window, 128 ops per batch, 1024-deep
    /// queue.
    pub const DEFAULT: PipelineConfig = PipelineConfig {
        batch_window_ms: 1,
        batch_size_max: 128,
        batch_queue_max: 1024,
    };
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Per-handle pipeline owning the lazy dispatcher.
///
/// `Pipeline` is `Send + Sync`. The dispatcher thread, when spawned,
/// runs independently of the calling thread; communication is via
/// [`crossbeam_channel`]. On drop, the pipeline runs the shutdown
/// protocol: send the shutdown signal, drop the work-queue sender so
/// the dispatcher sees `Disconnected` after draining, then wait on
/// `done_rx.recv_timeout(5s)` and join the thread.
pub(crate) struct Pipeline {
    config: PipelineConfig,
    /// `None` until the first batch submit, then `Some(...)` for the
    /// rest of this `Pipeline`'s lifetime (or until `Drop` runs).
    inner: Mutex<Option<DispatcherInner>>,
}

/// Channels and join handle owned by the pipeline once the dispatcher
/// has been spawned.
struct DispatcherInner {
    job_tx: Sender<BatchJob>,
    shutdown_tx: Sender<()>,
    done_rx: Receiver<()>,
    /// Wrapped in `Option` so `Pipeline`'s `Drop` impl can `take()`
    /// ownership of the [`JoinHandle`] for joining (or
    /// [`std::mem::forget`] on timeout).
    thread: Option<JoinHandle<()>>,
}

impl Pipeline {
    /// Constructs a new pipeline with the given configuration.
    ///
    /// The dispatcher thread is **not** spawned here. It is created on
    /// the first call to `submit`.
    pub(crate) fn new(config: PipelineConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(None),
        }
    }

    /// Returns the active configuration.
    #[allow(dead_code)] // used by Builder integration in checkpoint D
    pub(crate) fn config(&self) -> PipelineConfig {
        self.config
    }

    /// Submits a batch of pre-resolved ops to the group lane and blocks
    /// until the dispatcher reports completion.
    ///
    /// Backpressure: when the queue is full, the underlying
    /// `Sender::send` blocks until space is available (decision #4).
    ///
    /// Returns `Ok(())` if every op completed successfully. On the
    /// first failure (whether returned `Err` or panic), the dispatcher
    /// stops processing this batch and reports
    /// [`BatchError::failed_at`] / [`BatchError::completed`] /
    /// [`BatchError::source`]. Subsequent ops in the same batch are
    /// **not** attempted.
    ///
    /// # Errors
    ///
    /// - [`BatchError`] wrapping [`crate::Error::ShutdownInProgress`]
    ///   when the dispatcher cannot be reached (handle is being
    ///   dropped, or thread spawn failed).
    /// - [`BatchError`] wrapping the underlying [`crate::Error`] when
    ///   an op fails.
    pub(crate) fn submit(
        &self,
        ops: Vec<BatchOp>,
        snapshot: HandleSnapshot,
    ) -> std::result::Result<(), BatchError> {
        let job_tx = match self.dispatcher_sender() {
            Some(tx) => tx,
            None => return Err(shutdown_err()),
        };

        let (response_tx, response_rx) = bounded(1);
        let job = BatchJob {
            ops,
            snapshot,
            response: crate::pipeline::group::BatchResponse::Sync(response_tx),
        };

        // Bounded send: blocks when the queue is full; returns Err iff
        // the dispatcher has dropped its receivers (shutdown in flight).
        if job_tx.send(job).is_err() {
            return Err(shutdown_err());
        }

        match response_rx.recv() {
            Ok(result) => result,
            // Receiver disconnected: dispatcher exited without sending
            // a response. Treat as shutdown.
            Err(_) => Err(shutdown_err()),
        }
    }

    /// Async equivalent of [`Pipeline::submit`]. Routes through the
    /// same per-handle dispatcher; the response channel is a
    /// [`tokio::sync::oneshot`] so the caller `.await`s without
    /// blocking a tokio worker.
    ///
    /// The crossbeam queue submission itself remains synchronous —
    /// when the dispatcher's bounded queue is full, this method
    /// blocks the calling task on `crossbeam_channel::send` until
    /// space frees up. This honours the dispatcher's backpressure
    /// contract from D-1 of `0.4.0`. Spawning a buffer thread to
    /// keep submission async would defeat that backpressure.
    #[cfg(feature = "async")]
    pub(crate) async fn submit_async(
        &self,
        ops: Vec<BatchOp>,
        snapshot: HandleSnapshot,
    ) -> std::result::Result<(), BatchError> {
        let job_tx = match self.dispatcher_sender() {
            Some(tx) => tx,
            None => return Err(shutdown_err()),
        };

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let job = BatchJob {
            ops,
            snapshot,
            response: crate::pipeline::group::BatchResponse::Async(response_tx),
        };

        if job_tx.send(job).is_err() {
            return Err(shutdown_err());
        }

        match response_rx.await {
            Ok(result) => result,
            // Receiver disconnected: dispatcher exited without sending
            // a response. Same handling as the sync path.
            Err(_) => Err(shutdown_err()),
        }
    }

    /// Returns a clone of the dispatcher's job sender, spawning the
    /// dispatcher thread if it has not been spawned yet.
    ///
    /// Returns `None` only when [`Pipeline::drop`] has already
    /// taken the inner — at that point the pipeline is shutting down
    /// and any pending submit must fail with `ShutdownInProgress`.
    fn dispatcher_sender(&self) -> Option<Sender<BatchJob>> {
        // Tolerate poisoned mutex by recovering the inner — the lock is
        // never held across user code or potentially-panicking sections,
        // so poisoning here means the *prior* OS-level thread death
        // already happened; recovery is correct.
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if guard.is_none() {
            *guard = Some(spawn_dispatcher(self.config));
        }
        guard.as_ref().map(|i| i.job_tx.clone())
    }
}

/// Constructs an [`Error::ShutdownInProgress`] wrapped in a
/// [`BatchError`] with `failed_at = 0`, `completed = 0`.
fn shutdown_err() -> BatchError {
    BatchError {
        failed_at: 0,
        completed: 0,
        source: Box::new(crate::Error::ShutdownInProgress),
    }
}

/// Spawns the dispatcher thread and returns the channel handles owned
/// by the pipeline.
///
/// If the OS refuses to spawn a thread (rare — out of memory or hit
/// thread limit), the move-closure containing the receivers is dropped,
/// and any subsequent `submit` will see `Disconnected` and return
/// `ShutdownInProgress`. This is the same observable behaviour as a
/// dispatcher that has cleanly exited; the rest of the pipeline degrades
/// without panicking.
fn spawn_dispatcher(config: PipelineConfig) -> DispatcherInner {
    let (job_tx, job_rx) = bounded::<BatchJob>(config.batch_queue_max);
    let (shutdown_tx, shutdown_rx) = bounded::<()>(1);
    let (done_tx, done_rx) = bounded::<()>(1);

    let thread = thread::Builder::new()
        .name("fsys-dispatcher".into())
        .spawn(move || {
            group::run_dispatcher(config, job_rx, shutdown_rx, done_tx);
        })
        .ok();

    DispatcherInner {
        job_tx,
        shutdown_tx,
        done_rx,
        thread,
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // Tolerate poisoned mutex (see dispatcher_sender for rationale).
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let Some(mut inner) = guard.take() else {
            return;
        };
        // Drop the guard first so the dispatcher (if it's stalled trying
        // to acquire something — it shouldn't be, but defensive) is not
        // blocked by us.
        drop(guard);

        // Step 1: signal shutdown.
        let _ = inner.shutdown_tx.send(());
        // Step 2: drop the producer side so the dispatcher's `select!`
        // sees `Disconnected` after it has drained.
        drop(inner.job_tx);
        drop(inner.shutdown_tx);

        // Step 3: wait for the dispatcher's final ack with a 5s hard
        // timeout, then join. If the timeout elapses, forget the
        // JoinHandle so `Drop` does not block forever — this path is
        // defensive and never expected to trigger; documented diagnostic
        // gap arrives with `tracing` integration in 0.7.0.
        let dispatcher_acked = inner.done_rx.recv_timeout(Duration::from_secs(5)).is_ok();
        if let Some(thread) = inner.thread.take() {
            if dispatcher_acked {
                let _ = thread.join();
            } else {
                std::mem::forget(thread);
            }
        }
    }
}

// Compile-time assertion: Pipeline must be Send + Sync so it can live
// inside a Handle that is shared across threads.
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn check() {
        assert_send::<Pipeline>();
        assert_sync::<Pipeline>();
    }
    let _ = check;
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_pipeline_test_{}_{}_{}",
            std::process::id(),
            n,
            suffix
        ))
    }

    fn snapshot_default() -> HandleSnapshot {
        HandleSnapshot {
            method: crate::Method::Sync,
            sector_size: 512,
            use_direct: false,
        }
    }

    #[test]
    fn test_pipeline_config_default_matches_prompt() {
        let c = PipelineConfig::default();
        assert_eq!(c.batch_window_ms, 1);
        assert_eq!(c.batch_size_max, 128);
        assert_eq!(c.batch_queue_max, 1024);
    }

    #[test]
    fn test_pipeline_new_does_not_spawn_dispatcher() {
        // No batch submitted → dispatcher must not exist.
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        let guard = p.inner.lock().unwrap();
        assert!(guard.is_none(), "lazy spawn: dispatcher must not exist yet");
    }

    #[test]
    fn test_pipeline_submit_spawns_dispatcher_on_first_call() {
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        // Empty batch is a degenerate case but should still round-trip.
        let r = p.submit(Vec::new(), snapshot_default());
        assert!(r.is_ok(), "empty batch should succeed: {:?}", r);
        let guard = p.inner.lock().unwrap();
        assert!(guard.is_some(), "dispatcher must exist after first submit");
    }

    #[test]
    fn test_pipeline_submit_executes_a_single_write() {
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        let path = tmp_path("single_write");
        let _g = scopeguard_remove(path.clone());
        let ops = vec![BatchOp::Write {
            path: path.clone(),
            data: b"hello-pipeline".to_vec(),
        }];
        p.submit(ops, snapshot_default()).expect("submit");
        let actual = std::fs::read(&path).expect("read");
        assert_eq!(actual, b"hello-pipeline");
    }

    #[test]
    fn test_pipeline_submit_executes_multiple_writes_in_order() {
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        let path = tmp_path("multi_write");
        let _g = scopeguard_remove(path.clone());
        let ops = vec![
            BatchOp::Write {
                path: path.clone(),
                data: b"first".to_vec(),
            },
            BatchOp::Write {
                path: path.clone(),
                data: b"second".to_vec(),
            },
            BatchOp::Write {
                path: path.clone(),
                data: b"third".to_vec(),
            },
        ];
        p.submit(ops, snapshot_default()).expect("submit");
        // Strict input order → last write wins.
        assert_eq!(std::fs::read(&path).unwrap(), b"third");
    }

    #[test]
    fn test_pipeline_submit_delete_removes_file() {
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        let path = tmp_path("delete");
        std::fs::write(&path, b"to-be-deleted").unwrap();
        let ops = vec![BatchOp::Delete { path: path.clone() }];
        p.submit(ops, snapshot_default()).expect("submit");
        assert!(!path.exists(), "file should be gone");
    }

    #[test]
    fn test_pipeline_submit_copy_duplicates_content() {
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        let src = tmp_path("copy_src");
        let dst = tmp_path("copy_dst");
        let _g1 = scopeguard_remove(src.clone());
        let _g2 = scopeguard_remove(dst.clone());
        std::fs::write(&src, b"copy-me").unwrap();
        let ops = vec![BatchOp::Copy {
            src: src.clone(),
            dst: dst.clone(),
        }];
        p.submit(ops, snapshot_default()).expect("submit");
        assert_eq!(std::fs::read(&dst).unwrap(), b"copy-me");
    }

    #[test]
    fn test_pipeline_submit_reports_failure_at_correct_index() {
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        let good = tmp_path("good");
        let _g1 = scopeguard_remove(good.clone());
        // Use a path that cannot be written: an existing directory.
        let bad_dir = tmp_path("bad_dir");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let _g_dir = scopeguard_remove_dir(bad_dir.clone());
        let ops = vec![
            BatchOp::Write {
                path: good.clone(),
                data: b"ok".to_vec(),
            },
            BatchOp::Write {
                path: bad_dir.clone(), // should fail (target is a dir)
                data: b"fail".to_vec(),
            },
            BatchOp::Write {
                path: good.clone(),
                data: b"never".to_vec(),
            },
        ];
        let result = p.submit(ops, snapshot_default());
        let err = result.expect_err("expected failure on op 1");
        assert_eq!(err.failed_at, 1, "failed_at index");
        assert_eq!(err.completed, 1, "completed count");
        // Op 0 (good write) is durable; op 2 (never) was not attempted.
        assert_eq!(std::fs::read(&good).unwrap(), b"ok");
    }

    #[test]
    fn test_pipeline_drop_signals_dispatcher_to_exit() {
        let start = Instant::now();
        {
            let p = Pipeline::new(PipelineConfig::DEFAULT);
            // Spawn the dispatcher.
            p.submit(Vec::new(), snapshot_default()).unwrap();
        } // drop runs here
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drop should not stall — got {:?}",
            elapsed
        );
    }

    #[test]
    fn test_pipeline_handles_many_concurrent_submitters() {
        use std::sync::Arc;
        let p = Arc::new(Pipeline::new(PipelineConfig::DEFAULT));
        let path_base = tmp_path("concurrent");
        let mut handles = Vec::new();
        let n_threads = 8;
        let writes_per_thread = 4;
        for t in 0..n_threads {
            let p = Arc::clone(&p);
            let pb = path_base.clone();
            handles.push(std::thread::spawn(move || {
                for w in 0..writes_per_thread {
                    let path = PathBuf::from(format!("{}_t{}_w{}", pb.display(), t, w));
                    let ops = vec![BatchOp::Write {
                        path,
                        data: format!("t{}w{}", t, w).into_bytes(),
                    }];
                    p.submit(ops, snapshot_default()).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Verify every file landed.
        for t in 0..n_threads {
            for w in 0..writes_per_thread {
                let path = PathBuf::from(format!("{}_t{}_w{}", path_base.display(), t, w));
                let expected = format!("t{}w{}", t, w);
                let actual = std::fs::read_to_string(&path).unwrap();
                assert_eq!(actual, expected);
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    // ── Test scaffolding ─────────────────────────────────────────────────

    struct ScopeGuardFile(PathBuf);
    impl Drop for ScopeGuardFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn scopeguard_remove(p: PathBuf) -> ScopeGuardFile {
        ScopeGuardFile(p)
    }

    struct ScopeGuardDir(PathBuf);
    impl Drop for ScopeGuardDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn scopeguard_remove_dir(p: PathBuf) -> ScopeGuardDir {
        ScopeGuardDir(p)
    }
}
