//! Pipeline subsystem — solo and group lanes for IO dispatch.
//!
//! Every [`crate::Handle`] owns a `Pipeline` (this module's primary
//! type, kept `pub(crate)` because the pipeline is a crate-internal
//! concern; users interact with it indirectly via `Handle`'s batch
//! API). The pipeline implements the dual-lane model from `0.4.0`:
//!
//! - **Solo lane.** `Handle::write`, `read`, `append`, `write_at`,
//!   `delete`, `copy`, `meta`, `sync`, and the directory calls go
//!   straight into `crud::file` / `crud::dir`. The pipeline is *not
//!   consulted*; no code on the solo path may touch it.
//! - **Group lane.** `Handle::write_batch`, `delete_batch`, `copy_batch`,
//!   and `Batch::commit` route through this module. Ops are placed on a
//!   bounded MPMC queue (default 1024 jobs deep) and consumed by a
//!   per-handle dispatcher thread that executes them in strict
//!   submission order and reports per-batch results back to the
//!   caller. The dispatcher never waits for more work: each op has its
//!   own fence, so there is no fsync to share by delaying a batch.
//!   With the default single dispatcher, all batches submitted to one
//!   handle (from any thread) run one after another; use
//!   `Builder::dispatcher_shards` to run batches for different paths
//!   in parallel.
//!
//! Routing is explicit and fixed per call site (decision R-12 in
//! `.dev/DECISIONS-0.4.0.md`): batch APIs always use the group lane,
//! everything else the solo lane. There is no automatic promotion of
//! single ops to batches.
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::error::BatchError;

mod group;

use group::BatchJob;
pub(crate) use group::{BatchOp, HandleSnapshot};

/// Configuration knobs for the group-lane pipeline.
///
/// Set by the [`crate::Builder`] (`batch_size_max`, `batch_queue_max`,
/// `dispatcher_shards`). Defaults: 128 ops per dispatcher pass,
/// 1024-deep queue, 1 dispatcher (single-thread per handle, identical
/// to pre-0.9.3 behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PipelineConfig {
    /// Upper bound on the ops the dispatcher takes from the queue in
    /// one pass (it always takes at least one job). `0` means one job
    /// per pass. Jobs run in queue order either way; the bound only
    /// limits how much work a pass picks up at once.
    pub batch_size_max: usize,
    /// Bounded queue capacity. When the queue is full, callers
    /// submitting a batch *block* until space is available (decision #4
    /// — bounded queue with blocking submission). Each shard has its
    /// own queue of this capacity; with `dispatcher_shards = N` the
    /// aggregate queue depth is `N × batch_queue_max`.
    pub batch_queue_max: usize,
    /// 0.9.3: number of dispatcher threads per handle. Default `1`
    /// preserves pre-0.9.3 behaviour exactly (single-threaded
    /// dispatch, one queue, one thread). Values `> 1` spawn N
    /// dispatcher threads each with their own bounded queue; batches
    /// are routed to a shard via hash of the first op's path so all
    /// ops in one batch land on the same dispatcher (preserving the
    /// strict within-batch submission-order contract). Cross-batch
    /// ordering across shards is **not** guaranteed — but that was
    /// already the case for cross-batch ordering generally.
    /// Clamped to `1..=64`.
    pub dispatcher_shards: usize,
}

impl PipelineConfig {
    /// Default configuration. 128 ops per pass, 1024-deep queue per
    /// shard, 1 shard (single dispatcher).
    pub const DEFAULT: PipelineConfig = PipelineConfig {
        batch_size_max: 128,
        batch_queue_max: 1024,
        dispatcher_shards: 1,
    };
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Per-handle pipeline owning the lazy dispatcher(s).
///
/// `Pipeline` is `Send + Sync`. Dispatcher threads, when spawned, run
/// independently of the calling thread; communication is via
/// [`crossbeam_channel`]. On drop, the pipeline runs the shutdown
/// protocol on every shard: signal shutdown, drop the work-queue
/// sender, then wait on each `done_rx.recv_timeout(5s)` and join its
/// thread.
///
/// 0.9.3: holds one [`Shard`] per dispatcher. The default
/// `PipelineConfig::DEFAULT` sets `dispatcher_shards = 1`, so there is
/// one shard and behaviour is identical to pre-0.9.3. Higher shard
/// counts spawn N independent dispatchers hashed by the first op's
/// path; ops within one batch always land on the same shard.
pub(crate) struct Pipeline {
    config: PipelineConfig,
    /// Thread spawner. `std::thread::Builder` in production; tests
    /// inject failures to exercise the respawn path.
    spawn: SpawnFn,
    /// Empty until the first batch submit, then one [`Shard`] per
    /// dispatcher for the rest of this `Pipeline`'s lifetime. After
    /// initialisation, submits read it with a single atomic load;
    /// earlier versions locked a mutex on every submit.
    fleet: OnceLock<Vec<Shard>>,
}

/// Spawns a named dispatcher thread running `body`.
type SpawnFn =
    fn(name: String, body: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<JoinHandle<()>>;

fn spawn_os_thread(
    name: String,
    body: Box<dyn FnOnce() + Send + 'static>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name).spawn(body)
}

/// Channels and thread state of one dispatcher shard.
struct Shard {
    job_tx: Sender<BatchJob>,
    shutdown_tx: Sender<()>,
    done_rx: Receiver<()>,
    /// `true` once a dispatcher thread owns this shard's receivers.
    /// Checked on every submit; the mutex below is only taken while
    /// no thread is running.
    running: AtomicBool,
    /// Cold-path state: the dispatcher's join handle, plus the
    /// receiving ends a (re)spawn hands to a new thread.
    thread: Mutex<ShardThread>,
}

struct ShardThread {
    handle: Option<JoinHandle<()>>,
    job_rx: Receiver<BatchJob>,
    shutdown_rx: Receiver<()>,
    done_tx: Sender<()>,
}

impl Shard {
    fn new(config: PipelineConfig) -> Self {
        let (job_tx, job_rx) = bounded::<BatchJob>(config.batch_queue_max);
        let (shutdown_tx, shutdown_rx) = bounded::<()>(1);
        let (done_tx, done_rx) = bounded::<()>(1);
        Self {
            job_tx,
            shutdown_tx,
            done_rx,
            running: AtomicBool::new(false),
            thread: Mutex::new(ShardThread {
                handle: None,
                job_rx,
                shutdown_rx,
                done_tx,
            }),
        }
    }

    /// Makes sure a dispatcher thread serves this shard, spawning one
    /// if none is running. Returns `false` when the OS refuses the
    /// thread; the next submit tries again, so a transient spawn
    /// failure (thread or memory limit) does not disable the shard
    /// for the rest of the handle's life.
    fn ensure_running(&self, config: PipelineConfig, spawn: SpawnFn, name: &str) -> bool {
        if self.running.load(Ordering::Acquire) {
            return true;
        }
        // Tolerate a poisoned mutex: the lock is never held across
        // user code, so a poisoned guard still holds consistent state.
        let mut st = match self.thread.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if self.running.load(Ordering::Acquire) {
            return true;
        }
        let job_rx = st.job_rx.clone();
        let shutdown_rx = st.shutdown_rx.clone();
        let done_tx = st.done_tx.clone();
        let body = Box::new(move || {
            group::run_dispatcher(config, job_rx, shutdown_rx, done_tx);
        });
        match spawn(name.to_string(), body) {
            Ok(handle) => {
                st.handle = Some(handle);
                self.running.store(true, Ordering::Release);
                true
            }
            Err(_) => false,
        }
    }
}

impl Pipeline {
    /// Constructs a new pipeline with the given configuration.
    ///
    /// The dispatcher thread is **not** spawned here. It is created on
    /// the first call to `submit`.
    pub(crate) fn new(config: PipelineConfig) -> Self {
        Self::with_spawner(config, spawn_os_thread)
    }

    fn with_spawner(config: PipelineConfig, spawn: SpawnFn) -> Self {
        Self {
            config,
            spawn,
            fleet: OnceLock::new(),
        }
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
    ///   when no dispatcher thread can serve the batch (the OS refused
    ///   to spawn one for this submit) or the dispatcher exited.
    /// - [`BatchError`] wrapping the underlying [`crate::Error`] when
    ///   an op fails.
    pub(crate) fn submit(
        &self,
        ops: Vec<BatchOp>,
        snapshot: HandleSnapshot,
        grouped: bool,
    ) -> std::result::Result<(), BatchError> {
        let shard = pick_shard(&ops, self.config.dispatcher_shards);
        let Some(job_tx) = self.dispatcher_sender(shard) else {
            return Err(shutdown_err());
        };

        let (response_tx, response_rx) = bounded(1);
        let job = BatchJob {
            ops,
            snapshot,
            response: crate::pipeline::group::BatchResponse::Sync(response_tx),
            grouped,
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
    /// **Backpressure under saturation.** The job is offered with a
    /// non-blocking `try_send`. When the dispatcher's bounded queue is
    /// full, the blocking `send` is moved to tokio's blocking pool
    /// (`spawn_blocking`) and awaited: the calling task is suspended
    /// until space appears, the runtime worker stays free, and no CPU
    /// is spent polling. (Before 1.1.1 this path spun on `try_send` +
    /// `yield_now`, burning a worker while the queue stayed full.) It
    /// needs no timer, so it works on runtimes built without
    /// `enable_time`.
    #[cfg(feature = "async")]
    pub(crate) async fn submit_async(
        &self,
        ops: Vec<BatchOp>,
        snapshot: HandleSnapshot,
        grouped: bool,
    ) -> std::result::Result<(), BatchError> {
        let shard = pick_shard(&ops, self.config.dispatcher_shards);
        let Some(job_tx) = self.dispatcher_sender(shard) else {
            return Err(shutdown_err());
        };

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let job = BatchJob {
            ops,
            snapshot,
            response: crate::pipeline::group::BatchResponse::Async(response_tx),
            grouped,
        };

        match job_tx.try_send(job) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(job)) => {
                let tx = job_tx.clone();
                let sent = tokio::task::spawn_blocking(move || tx.send(job).is_ok())
                    .await
                    .unwrap_or(false);
                if !sent {
                    return Err(shutdown_err());
                }
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                return Err(shutdown_err());
            }
        }

        match response_rx.await {
            Ok(result) => result,
            // Receiver disconnected: dispatcher exited without sending
            // a response. Same handling as the sync path.
            Err(_) => Err(shutdown_err()),
        }
    }

    /// Returns the requested shard's job sender, creating the
    /// dispatcher fleet on first use and (re)spawning the shard's
    /// thread if none is running.
    ///
    /// 0.9.3: all N shards are spawned together on the first batch
    /// submit. `shard` is the destination shard index (`< N`) chosen
    /// by [`pick_shard`].
    ///
    /// Returns `None` when no dispatcher thread can be spawned for the
    /// shard right now.
    fn dispatcher_sender(&self, shard: usize) -> Option<&Sender<BatchJob>> {
        let fleet = self.fleet.get_or_init(|| {
            let fleet: Vec<Shard> = (0..self.config.dispatcher_shards.max(1))
                .map(|_| Shard::new(self.config))
                .collect();
            let n = fleet.len();
            for (idx, s) in fleet.iter().enumerate() {
                // A failure here is retried by the submit below and by
                // every later submit to that shard.
                let _ = s.ensure_running(self.config, self.spawn, &shard_name(idx, n));
            }
            fleet
        });
        let s = fleet.get(shard)?;
        if s.ensure_running(self.config, self.spawn, &shard_name(shard, fleet.len())) {
            Some(&s.job_tx)
        } else {
            None
        }
    }
}

/// Thread name for shard `idx` of `n`. The single-shard default keeps
/// the original `fsys-dispatcher` name so observability tooling that
/// pinned to it doesn't break.
fn shard_name(idx: usize, n: usize) -> String {
    if n == 1 {
        "fsys-dispatcher".to_string()
    } else {
        format!("fsys-dispatcher-{idx}")
    }
}

/// 0.9.3: picks the destination shard for a batch.
///
/// Hashes the first op's primary path so all ops in a batch land on
/// the same shard — preserves the within-batch submission-order
/// contract (one dispatcher per shard executes its work serially).
/// Empty batches map to shard `0`. With `n_shards = 1` returns `0`
/// unconditionally without hashing.
fn pick_shard(ops: &[BatchOp], n_shards: usize) -> usize {
    if n_shards <= 1 {
        return 0;
    }
    let Some(first) = ops.first() else { return 0 };
    let path: &std::path::Path = match first {
        BatchOp::Write { path, .. } => path,
        BatchOp::Delete { path } => path,
        BatchOp::Copy { src, .. } => src,
    };
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    (h.finish() % n_shards as u64) as usize
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

impl Drop for Pipeline {
    fn drop(&mut self) {
        let Some(fleet) = self.fleet.take() else {
            return;
        };

        // Step 1: signal shutdown to every shard in parallel.
        for shard in &fleet {
            let _ = shard.shutdown_tx.send(());
        }

        // Step 2: release every producer-side handle we hold (the job
        // sender, and the receiver clones kept for respawning), then
        // wait for each running dispatcher's final ack with a 5 s hard
        // timeout and join it. On timeout the thread's JoinHandle is
        // forgotten so `Drop` does not block forever; remaining shards
        // still get joined cleanly.
        for shard in fleet {
            let Shard {
                job_tx,
                shutdown_tx,
                done_rx,
                running: _,
                thread,
            } = shard;
            drop(job_tx);
            drop(shutdown_tx);
            let ShardThread {
                handle,
                job_rx,
                shutdown_rx,
                done_tx,
            } = match thread.into_inner() {
                Ok(t) => t,
                Err(p) => p.into_inner(),
            };
            drop((job_rx, shutdown_rx, done_tx));
            if let Some(handle) = handle {
                if done_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                    let _ = handle.join();
                } else {
                    std::mem::forget(handle);
                }
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
        assert_eq!(c.batch_size_max, 128);
        assert_eq!(c.batch_queue_max, 1024);
        // 0.9.3: default shard count = 1 (single-dispatcher).
        assert_eq!(c.dispatcher_shards, 1);
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.3 — Sharded dispatcher coverage
    // ─────────────────────────────────────────────────────────

    #[test]
    fn test_pick_shard_returns_zero_when_unsharded() {
        // dispatcher_shards = 1 → always shard 0 (no hashing).
        assert_eq!(pick_shard(&[], 1), 0);
        let ops = vec![BatchOp::Write {
            path: PathBuf::from("/x"),
            data: vec![],
        }];
        assert_eq!(pick_shard(&ops, 1), 0);
    }

    #[test]
    fn test_pick_shard_empty_batch_maps_to_zero() {
        // Empty batches always go to shard 0 regardless of N.
        assert_eq!(pick_shard(&[], 8), 0);
    }

    #[test]
    fn test_pick_shard_is_deterministic_per_path() {
        // The same path must always hash to the same shard.
        let ops = vec![BatchOp::Write {
            path: PathBuf::from("/data/segment.00001"),
            data: vec![],
        }];
        let a = pick_shard(&ops, 16);
        let b = pick_shard(&ops, 16);
        assert_eq!(a, b);
        assert!(a < 16);
    }

    #[test]
    fn test_pick_shard_uses_first_op_path() {
        // Two batches with different first-op paths land on
        // (possibly) different shards. We can't assert they
        // differ — hash collisions exist — but we can confirm
        // the function reads the first op's path.
        let a = vec![BatchOp::Write {
            path: PathBuf::from("/data/a"),
            data: vec![],
        }];
        let b = vec![BatchOp::Delete {
            path: PathBuf::from("/data/a"),
        }];
        // Same path string → same shard regardless of op variant.
        assert_eq!(pick_shard(&a, 8), pick_shard(&b, 8));
    }

    #[test]
    fn test_pick_shard_copy_uses_src_path() {
        let copy = vec![BatchOp::Copy {
            src: PathBuf::from("/data/src"),
            dst: PathBuf::from("/elsewhere/dst"),
        }];
        let write = vec![BatchOp::Write {
            path: PathBuf::from("/data/src"),
            data: vec![],
        }];
        assert_eq!(pick_shard(&copy, 8), pick_shard(&write, 8));
    }

    #[test]
    fn test_pipeline_spawns_n_dispatchers_when_sharded() {
        // dispatcher_shards = 4 → first submit must spawn 4
        // independent dispatchers.
        let p = Pipeline::new(PipelineConfig {
            dispatcher_shards: 4,
            ..PipelineConfig::DEFAULT
        });
        p.submit(Vec::new(), snapshot_default(), false)
            .expect("submit");
        let fleet = p.fleet.get().expect("fleet must exist");
        assert_eq!(fleet.len(), 4);
        // Every shard must have a live thread.
        for shard in fleet.iter() {
            assert!(shard.running.load(Ordering::Acquire));
        }
    }

    #[test]
    fn test_multi_shard_executes_writes_across_paths() {
        // End-to-end: 4 shards, 16 writes to distinct paths. Every
        // write must land on disk regardless of which shard handled
        // it.
        let p = Pipeline::new(PipelineConfig {
            dispatcher_shards: 4,
            ..PipelineConfig::DEFAULT
        });
        let mut paths = Vec::new();
        for i in 0..16 {
            let path = tmp_path(&format!("multi_shard_{i:02}"));
            paths.push(path.clone());
            p.submit(
                vec![BatchOp::Write {
                    path,
                    data: format!("payload-{i:02}").into_bytes(),
                }],
                snapshot_default(),
                false,
            )
            .expect("submit");
        }
        for (i, path) in paths.iter().enumerate() {
            let bytes = std::fs::read(path).expect("read");
            assert_eq!(bytes, format!("payload-{i:02}").into_bytes());
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn test_multi_shard_concurrent_submitters() {
        // 8 threads each submit 16 batches concurrently against a
        // 4-shard pipeline. All ops must complete; no deadlock; no
        // shard left behind.
        use std::sync::Arc;
        let p = Arc::new(Pipeline::new(PipelineConfig {
            dispatcher_shards: 4,
            ..PipelineConfig::DEFAULT
        }));
        let start = Instant::now();
        let mut handles = Vec::new();
        for t in 0..8u32 {
            let p = p.clone();
            handles.push(std::thread::spawn(move || {
                let mut written = Vec::new();
                for i in 0..16u32 {
                    let path = tmp_path(&format!("concurrent_t{t:02}_i{i:02}"));
                    p.submit(
                        vec![BatchOp::Write {
                            path: path.clone(),
                            data: format!("t{t}-i{i}").into_bytes(),
                        }],
                        snapshot_default(),
                        false,
                    )
                    .expect("submit");
                    written.push(path);
                }
                written
            }));
        }
        let mut all_paths = Vec::new();
        for h in handles {
            all_paths.extend(h.join().expect("join"));
        }
        assert_eq!(all_paths.len(), 128);
        for p in &all_paths {
            assert!(std::fs::metadata(p).is_ok(), "missing: {}", p.display());
            let _ = std::fs::remove_file(p);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(30),
            "multi-shard concurrent test exceeded 30s: {elapsed:?}"
        );
    }

    #[test]
    fn test_multi_shard_clean_shutdown_drains_all_shards() {
        // 4 shards, submit batches across multiple paths, then
        // drop the pipeline. Drop must signal shutdown and join
        // every shard's thread within the 5s budget.
        let p = Pipeline::new(PipelineConfig {
            dispatcher_shards: 4,
            ..PipelineConfig::DEFAULT
        });
        for i in 0..16 {
            let path = tmp_path(&format!("drop_shard_{i:02}"));
            p.submit(
                vec![BatchOp::Write {
                    path: path.clone(),
                    data: vec![],
                }],
                snapshot_default(),
                false,
            )
            .expect("submit");
            let _ = std::fs::remove_file(&path);
        }
        let start = Instant::now();
        drop(p);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "drop must complete within 5s; took {elapsed:?}"
        );
    }

    #[test]
    fn test_pipeline_new_does_not_spawn_dispatcher() {
        // No batch submitted → dispatcher must not exist.
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        assert!(
            p.fleet.get().is_none(),
            "lazy spawn: dispatcher must not exist yet"
        );
    }

    #[test]
    fn test_pipeline_submit_spawns_dispatcher_on_first_call() {
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        // Empty batch is a degenerate case but should still round-trip.
        let r = p.submit(Vec::new(), snapshot_default(), false);
        assert!(r.is_ok(), "empty batch should succeed: {:?}", r);
        assert!(
            p.fleet.get().is_some(),
            "dispatcher must exist after first submit"
        );
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
        p.submit(ops, snapshot_default(), false).expect("submit");
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
        p.submit(ops, snapshot_default(), false).expect("submit");
        // Strict input order → last write wins.
        assert_eq!(std::fs::read(&path).unwrap(), b"third");
    }

    #[test]
    fn test_pipeline_submit_delete_removes_file() {
        let p = Pipeline::new(PipelineConfig::DEFAULT);
        let path = tmp_path("delete");
        std::fs::write(&path, b"to-be-deleted").unwrap();
        let ops = vec![BatchOp::Delete { path: path.clone() }];
        p.submit(ops, snapshot_default(), false).expect("submit");
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
        p.submit(ops, snapshot_default(), false).expect("submit");
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
        let result = p.submit(ops, snapshot_default(), false);
        let err = result.expect_err("expected failure on op 1");
        assert_eq!(err.failed_at, 1, "failed_at index");
        assert_eq!(err.completed, 1, "completed count");
        // Op 0 (good write) is durable; op 2 (never) was not attempted.
        assert_eq!(std::fs::read(&good).unwrap(), b"ok");
    }

    #[test]
    fn test_pipeline_dispatches_queued_jobs_without_waiting() {
        // Two submitters racing used to make the dispatcher hold the
        // second job for the batch window although nothing was shared
        // between the jobs. A long window must not delay completion.
        use std::sync::Arc;
        let p = Arc::new(Pipeline::new(PipelineConfig::DEFAULT));
        p.submit(Vec::new(), snapshot_default(), false)
            .expect("spawn");
        let start = Instant::now();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let p = Arc::clone(&p);
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        p.submit(Vec::new(), snapshot_default(), false)
                            .expect("submit");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }
        // 100 empty jobs with no wait complete far below a second.
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn test_pipeline_with_rendezvous_queue_executes_batches() {
        // batch_queue_max(0) gives a zero-capacity (rendezvous) queue:
        // each submit hands its job directly to the dispatcher.
        let p = Pipeline::new(PipelineConfig {
            batch_queue_max: 0,
            ..PipelineConfig::DEFAULT
        });
        let path = tmp_path("rendezvous");
        let _g = scopeguard_remove(path.clone());
        for i in 0..3u8 {
            p.submit(
                vec![BatchOp::Write {
                    path: path.clone(),
                    data: vec![i],
                }],
                snapshot_default(),
                false,
            )
            .expect("submit through rendezvous queue");
        }
        assert_eq!(std::fs::read(&path).unwrap(), vec![2]);
    }

    #[test]
    fn test_pipeline_with_zero_batch_size_executes_batches() {
        let p = Pipeline::new(PipelineConfig {
            batch_size_max: 0,
            ..PipelineConfig::DEFAULT
        });
        let path = tmp_path("zero_batch_size");
        let _g = scopeguard_remove(path.clone());
        p.submit(
            vec![BatchOp::Write {
                path: path.clone(),
                data: b"z".to_vec(),
            }],
            snapshot_default(),
            false,
        )
        .expect("submit");
        assert_eq!(std::fs::read(&path).unwrap(), b"z");
    }

    static SPAWN_FAILURES_LEFT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn flaky_spawn(
        name: String,
        body: Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<JoinHandle<()>> {
        let left = SPAWN_FAILURES_LEFT.load(Ordering::SeqCst);
        if left > 0 {
            SPAWN_FAILURES_LEFT.store(left - 1, Ordering::SeqCst);
            return Err(std::io::Error::other("injected spawn failure"));
        }
        spawn_os_thread(name, body)
    }

    #[test]
    fn test_pipeline_retries_dispatcher_spawn_after_failure() {
        // Before 1.1.1 a failed spawn stored `thread: None` for good and
        // every later batch on that shard failed with
        // ShutdownInProgress.
        SPAWN_FAILURES_LEFT.store(2, Ordering::SeqCst);
        let p = Pipeline::with_spawner(PipelineConfig::DEFAULT, flaky_spawn);
        // Fleet init attempt + this submit's retry both fail.
        let err = p
            .submit(Vec::new(), snapshot_default(), false)
            .expect_err("no dispatcher while spawning fails");
        assert!(matches!(*err.source, crate::Error::ShutdownInProgress));
        // The OS recovers: the next submit spawns the dispatcher.
        let path = tmp_path("respawn");
        let _g = scopeguard_remove(path.clone());
        p.submit(
            vec![BatchOp::Write {
                path: path.clone(),
                data: b"after-respawn".to_vec(),
            }],
            snapshot_default(),
            false,
        )
        .expect("submit after the spawn failure clears");
        assert_eq!(std::fs::read(&path).unwrap(), b"after-respawn");
        drop(p); // joins the respawned dispatcher
    }

    #[test]
    fn test_pipeline_drop_without_running_dispatcher_does_not_block() {
        let p = Pipeline::with_spawner(PipelineConfig::DEFAULT, |_, _| {
            Err(std::io::Error::other("never spawns"))
        });
        assert!(p.submit(Vec::new(), snapshot_default(), false).is_err());
        let start = Instant::now();
        drop(p);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(feature = "async")]
    #[test]
    fn test_submit_async_waits_for_queue_space_on_current_thread_runtime() {
        // A one-slot queue saturated by many async submitters on a
        // single-threaded runtime: every batch must complete.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime without timers");
        let p = std::sync::Arc::new(Pipeline::new(PipelineConfig {
            batch_queue_max: 1,
            ..PipelineConfig::DEFAULT
        }));
        let base = tmp_path("async_full");
        rt.block_on(async {
            let mut tasks = Vec::new();
            for i in 0..32u32 {
                let p = std::sync::Arc::clone(&p);
                let path = PathBuf::from(format!("{}_{i}", base.display()));
                tasks.push(tokio::spawn(async move {
                    p.submit_async(
                        vec![BatchOp::Write {
                            path: path.clone(),
                            data: i.to_le_bytes().to_vec(),
                        }],
                        snapshot_default(),
                        false,
                    )
                    .await
                    .map(|()| path)
                }));
            }
            for t in tasks {
                let path = t.await.expect("join").expect("async submit");
                let _ = std::fs::remove_file(path);
            }
        });
    }

    #[test]
    fn test_pipeline_drop_signals_dispatcher_to_exit() {
        let start = Instant::now();
        {
            let p = Pipeline::new(PipelineConfig::DEFAULT);
            // Spawn the dispatcher.
            p.submit(Vec::new(), snapshot_default(), false).unwrap();
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
                    p.submit(ops, snapshot_default(), false).unwrap();
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
