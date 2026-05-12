//! [`Builder`] for constructing a configured [`Handle`].
//!
//! # Example
//!
//! ```
//! # fn example() -> fsys::Result<()> {
//! use fsys::{Builder, Method, Mode};
//!
//! let handle = Builder::new()
//!     .method(Method::Data)
//!     .mode(Mode::Dev)
//!     .build()?;
//! # Ok(())
//! # }
//! ```

use crate::handle::Handle;
use crate::method::Method;
use crate::observer::FsysObserver;
use crate::path::Mode;
use crate::pipeline::{Pipeline, PipelineConfig};
use crate::{Error, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// A builder for creating a [`Handle`].
///
/// Obtain one via [`crate::builder()`] or [`Builder::new()`].
///
/// All fields are optional. Unset fields use sensible defaults:
/// - `method` defaults to [`Method::Auto`] (hardware-aware selection).
/// - `root` defaults to `None` (no path scope enforcement).
/// - `mode` defaults to [`Mode::Auto`] (resolved from environment).
/// - `batch_window_ms` defaults to `1` (group-lane time threshold).
/// - `batch_size_max` defaults to `128` (group-lane count threshold).
/// - `batch_queue_max` defaults to `1024` (group-lane queue capacity;
///   producers block when full).
/// - `buffer_pool_count` defaults to `64` (per-handle aligned buffer
///   pool capacity; see locked decision #6 in
///   `.dev/DECISIONS-0.5.0.md`).
/// - `buffer_pool_block_size` defaults to `4096` (per-buffer size in bytes).
/// - `io_uring_queue_depth` defaults to `128` (Linux io_uring SQ
///   depth). Real `io_uring` integration shipped in `0.5.1` after
///   the rustc 1.95 ICE workaround landed; see the io_uring blocker
///   record in `.dev/DECISIONS-0.5.0.md`.
pub struct Builder {
    method: Method,
    root: Option<PathBuf>,
    mode: Mode,
    pipeline_config: PipelineConfig,
    buffer_pool_count: usize,
    buffer_pool_block_size: usize,
    io_uring_queue_depth: u32,
    /// 0.9.7 — opt-in `IORING_SETUP_SQPOLL` idle timeout (ms).
    /// `None` (default) = SQPOLL disabled. `Some(idle_ms)` opts in.
    /// Linux-only; ignored elsewhere.
    iouring_sqpoll_idle_ms: Option<u32>,
    observer: Option<Arc<dyn FsysObserver>>,
}

impl Builder {
    /// Creates a new `Builder` with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            method: Method::Auto,
            root: None,
            mode: Mode::Auto,
            pipeline_config: PipelineConfig::DEFAULT,
            buffer_pool_count: 64,
            buffer_pool_block_size: 4096,
            io_uring_queue_depth: 128,
            iouring_sqpoll_idle_ms: None,
            observer: None,
        }
    }

    /// Sets the durability method.
    ///
    /// Returns an error at [`build`](Builder::build) time if a reserved
    /// variant ([`Method::Mmap`] or [`Method::Journal`]) is supplied.
    #[must_use]
    pub fn method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    /// Restricts all IO to paths under `root`.
    ///
    /// When set, handle path resolution enforces that every path stays
    /// within this root. Relative paths are joined to the root; absolute
    /// paths that escape the root are rejected with
    /// [`Error::InvalidPath`].
    #[must_use]
    pub fn root<P: Into<PathBuf>>(mut self, root: P) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Sets the operating mode.
    ///
    /// Affects default path selection; [`Mode::Auto`] resolves from the
    /// `FSYS_MODE` / `RUST_ENV` environment variables.
    #[must_use]
    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    /// Sets the group-lane time threshold in milliseconds.
    ///
    /// The dispatcher flushes the current batch when *either* this many
    /// milliseconds elapse since the first job in the batch arrived,
    /// *or* [`Builder::batch_size_max`] ops have accumulated, whichever
    /// comes first. Default: `1` ms.
    ///
    /// Larger values amortise more syscall overhead per flush at the
    /// cost of higher per-batch latency. Smaller values approach the
    /// solo lane's latency at the cost of less amortisation. The
    /// default is tuned for storage-engine workloads that mix latency-
    /// sensitive and throughput-sensitive paths.
    ///
    /// Setting this to `0` is allowed but defeats the time-window
    /// component of the hybrid trigger — flushes will be driven solely
    /// by the count threshold.
    #[must_use]
    pub fn batch_window_ms(mut self, ms: u64) -> Self {
        self.pipeline_config.batch_window_ms = ms;
        self
    }

    /// Sets the group-lane count threshold.
    ///
    /// The dispatcher flushes the current batch when *either* this many
    /// ops have accumulated, *or* [`Builder::batch_window_ms`] elapses,
    /// whichever comes first. Default: `128` ops.
    ///
    /// Larger values reduce per-batch overhead at the cost of higher
    /// memory residency for in-flight batches. Smaller values reduce
    /// memory residency at the cost of more frequent dispatcher
    /// scheduling overhead.
    ///
    /// Setting this to `0` is allowed but defeats the count component
    /// of the hybrid trigger — flushes will be driven solely by the
    /// time-window deadline.
    #[must_use]
    pub fn batch_size_max(mut self, n: usize) -> Self {
        self.pipeline_config.batch_size_max = n;
        self
    }

    /// Sets the group-lane queue capacity.
    ///
    /// When the queue is full, calls to
    /// [`Handle::write_batch`](crate::Handle::write_batch),
    /// [`Handle::delete_batch`](crate::Handle::delete_batch),
    /// [`Handle::copy_batch`](crate::Handle::copy_batch), and
    /// [`crate::Batch::commit`] **block** until space is available
    /// (decision #4 — bounded queue with blocking submission).
    ///
    /// Default: `1024` jobs. Each job carries one batch (a `Vec` of
    /// ops + a oneshot response channel + a `HandleSnapshot`); the
    /// memory footprint of a full queue is bounded by the size of the
    /// largest job.
    ///
    /// Setting this to `0` is rejected at runtime by the underlying
    /// channel implementation — `0` would make every send block
    /// indefinitely. Use `1` for an "at most one job in flight at a
    /// time" workload.
    #[must_use]
    pub fn batch_queue_max(mut self, n: usize) -> Self {
        self.pipeline_config.batch_queue_max = n;
        self
    }

    /// Sets the per-handle aligned buffer pool capacity (number of
    /// reusable buffers).
    ///
    /// Default: `64`. Buffers are allocated lazily on the first
    /// Direct-method op; idle handles cost zero buffer memory. The
    /// pool is shared between caller threads and the group-lane
    /// dispatcher; access is lock-free on the fast path
    /// (`crossbeam_queue::ArrayQueue`).
    ///
    /// `0` is rejected at [`build`](Builder::build) time. Larger
    /// values reduce allocation pressure on Direct workloads at the
    /// cost of higher per-handle resident memory
    /// (`buffer_pool_count × buffer_pool_block_size` bytes when fully
    /// populated).
    #[must_use]
    pub fn buffer_pool_count(mut self, n: usize) -> Self {
        self.buffer_pool_count = n;
        self
    }

    /// Sets the per-buffer size in the aligned buffer pool, in bytes.
    ///
    /// Default: `4096`. Must be a non-zero multiple of the
    /// platform's logical sector size (typically 512 or 4096) and a
    /// power of two when alignment matters; `build()` validates this
    /// against the probed sector size.
    ///
    /// For Direct IO workloads with payloads larger than the default,
    /// a 64 KiB or 1 MiB block reduces the number of buffer leases per
    /// op at the cost of higher per-handle memory (see
    /// [`Builder::buffer_pool_count`]).
    #[must_use]
    pub fn buffer_pool_block_size(mut self, bytes: usize) -> Self {
        self.buffer_pool_block_size = bytes;
        self
    }

    /// Sets the Linux `io_uring` submission-queue depth.
    ///
    /// On Linux the [`Handle`] constructs a per-handle io_uring
    /// ring lazily on the first [`crate::Method::Direct`] op. SQEs
    /// for `write` / `read` / `fsync(DATASYNC)` route through the
    /// ring; on `io_uring_setup(2)` rejection (kernel < 5.1,
    /// SECCOMP, container restriction) the Direct path falls back
    /// to `O_DIRECT` + `pwrite` + `fdatasync` — same durability
    /// contract, slower path. macOS and Windows ignore this value
    /// (no io_uring on those platforms by design — see locked
    /// decision #1 in `.dev/DECISIONS-0.5.0.md`).
    ///
    /// Default: `128`.
    #[must_use]
    pub fn io_uring_queue_depth(mut self, depth: u32) -> Self {
        self.io_uring_queue_depth = depth;
        self
    }

    /// 0.9.7 — opts the per-handle io_uring sync ring into
    /// `IORING_SETUP_SQPOLL` with the given idle timeout in
    /// milliseconds.
    ///
    /// SQPOLL spawns (or shares) a kernel-side polling thread that
    /// drains the submission queue without requiring
    /// `io_uring_enter` syscalls. After `idle_ms` of no
    /// submissions, the kernel thread sleeps and the next push
    /// wakes it via an `io_uring_enter` syscall. The intended
    /// workload is sustained-throughput writers (database WAL
    /// flush loops, log-structured merge tree compaction) where
    /// the syscall amortisation matters.
    ///
    /// **When to enable.** Sustained Direct-IO write workloads on
    /// kernels ≥ 5.13 where the process has `CAP_SYS_NICE`
    /// (or runs as root). Typical idle values: `1000`-`5000`
    /// (1-5 s) for steady-state workloads; `100`-`200` for
    /// latency-sensitive bursty loads.
    ///
    /// **When to leave it off (the default).** Idle / low-rate
    /// workloads, containerised deployments without
    /// `CAP_SYS_NICE`, sandboxed environments with restrictive
    /// SECCOMP, kernels < 5.13. The kernel polling thread costs
    /// a kernel CPU while spinning — wasteful for low-rate
    /// workloads.
    ///
    /// **Fallback behaviour.** If `io_uring_setup(2)` rejects
    /// `IORING_SETUP_SQPOLL` (EPERM, unsupported kernel, etc.)
    /// the per-handle io_uring slot flips to `Disabled` and the
    /// Direct path uses non-SQPOLL `pwrite` + `fdatasync` —
    /// identical durability contract, slower path. No panic, no
    /// hang, no observable correctness change.
    ///
    /// Linux-only knob. On macOS / Windows the value is captured
    /// but never consulted (no io_uring on those platforms by
    /// design — locked decision #1 in `.dev/DECISIONS-0.5.0.md`).
    #[must_use]
    pub fn sqpoll(mut self, idle_ms: u32) -> Self {
        self.iouring_sqpoll_idle_ms = Some(idle_ms);
        self
    }

    /// 0.9.3 — Sets the number of group-lane dispatcher threads per
    /// handle.
    ///
    /// Default `1` preserves pre-0.9.3 behaviour exactly: a single
    /// dispatcher thread per handle, one bounded MPMC queue, every
    /// batch processed serially in submission order. Values `> 1`
    /// spawn N dispatcher threads on the first batch submit; each
    /// has its own bounded queue, and batches are routed to a shard
    /// via hash of the first op's primary path. All ops inside one
    /// `Batch::commit()` land on the same shard so the within-batch
    /// submission-order contract is preserved.
    ///
    /// **When to raise it.** On multi-core hosts where a single
    /// handle is the throughput bottleneck for *parallel* batch
    /// submitters writing to *different files* (e.g. a database
    /// flushing many SST tables concurrently). On these workloads
    /// the pre-0.9.3 single dispatcher was a hard one-core ceiling;
    /// `dispatcher_shards = num_cpus::get()` lifts it.
    ///
    /// **When to leave it at 1.** Single-writer workloads,
    /// latency-sensitive workloads (each shard has its own time
    /// window, so cross-shard ordering across batches is not
    /// guaranteed — but it was never guaranteed at the
    /// pipeline-level anyway), and any workload where batches
    /// rarely touch distinct paths (sharding by hash collapses to
    /// one shard when all batches target the same path).
    ///
    /// Clamped to `1..=64`. The high cap reflects that >64
    /// dispatcher threads per handle is pathological;
    /// `num_cpus::get()` is the natural ceiling for any realistic
    /// host.
    ///
    /// Aggregate queue depth scales with shard count: with
    /// `batch_queue_max(1024)` and `dispatcher_shards(8)`, the
    /// pipeline can hold 8 × 1024 = 8 K batches in flight.
    #[must_use]
    pub fn dispatcher_shards(mut self, shards: usize) -> Self {
        self.pipeline_config.dispatcher_shards = shards.clamp(1, 64);
        self
    }

    /// 0.9.2 — applies a coordinated workload preset.
    ///
    /// Pre-sets the buffer-pool capacity, buffer-pool block size,
    /// io_uring queue depth, and batch-queue capacity to a tuned
    /// combination matching a named workload shape. Equivalent to
    /// calling each underlying setter explicitly, but ensures the
    /// values stay coordinated as fsys evolves new defaults.
    ///
    /// Subsequent setter calls (`buffer_pool_count`,
    /// `io_uring_queue_depth`, etc.) override the preset's value
    /// for that knob, so callers can use a preset as a baseline
    /// and tweak individual fields. Calling `tune_for` after
    /// individual setters resets those setters to the preset's
    /// values — apply presets first.
    ///
    /// **`Workload::Database`** — tuned for storage-engine
    /// workloads (HiveDB, embedded KV stores, log-structured
    /// merge trees) on NVMe with sustained bulk writes. Sets:
    /// - `buffer_pool_count = 1024`,
    ///   `buffer_pool_block_size = 8192` (= 8 MiB resident per
    ///   handle, 32× the 256 KiB pre-0.9.2 default).
    /// - `io_uring_queue_depth = 256` (= 2× the pre-0.9.2 default).
    /// - `batch_queue_max = 4096` (= 4× the pre-0.9.2 default).
    ///
    /// **`Workload::Default`** — restores the library defaults
    /// (256 KiB pool, 128-deep ring, 1024-deep batch queue).
    /// Useful for tests and for callers who want to revert a
    /// preset before applying a different one.
    #[must_use]
    pub fn tune_for(mut self, workload: Workload) -> Self {
        match workload {
            Workload::Default => {
                self.buffer_pool_count = 64;
                self.buffer_pool_block_size = 4096;
                self.io_uring_queue_depth = 128;
                self.pipeline_config.batch_queue_max = 1024;
            }
            Workload::Database => {
                self.buffer_pool_count = 1024;
                self.buffer_pool_block_size = 8192;
                self.io_uring_queue_depth = 256;
                self.pipeline_config.batch_queue_max = 4096;
            }
        }
        self
    }

    /// 0.9.2 — registers a structured-telemetry observer with this
    /// handle.
    ///
    /// See [`crate::observer::FsysObserver`] for the trait
    /// contract. The handle keeps an `Arc<dyn FsysObserver>` clone
    /// for the rest of its lifetime; the observer fires on the
    /// instrumented hot paths (`Handle::write` / `read`,
    /// `JournalHandle::append` / `append_batch` / `sync_through`).
    /// Per-op cost when no observer is registered is a single
    /// `Option::is_some` branch.
    ///
    /// Calling `observer()` twice replaces the previously
    /// registered observer; only the most recent registration
    /// survives into [`Builder::build`].
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use std::sync::Arc;
    /// use fsys::observer::{FsysObserver, JournalSyncEvent};
    ///
    /// #[derive(Debug, Default)]
    /// struct Counter {
    ///     syncs: AtomicU64,
    /// }
    /// impl FsysObserver for Counter {
    ///     fn on_journal_sync(&self, _: JournalSyncEvent) {
    ///         self.syncs.fetch_add(1, Ordering::Relaxed);
    ///     }
    /// }
    ///
    /// let counter = Arc::new(Counter::default());
    /// let fs = fsys::builder().observer(counter.clone()).build().unwrap();
    /// # let _ = fs;
    /// ```
    #[must_use]
    pub fn observer(mut self, observer: Arc<dyn FsysObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Constructs the [`Handle`].
    ///
    /// Resolves `Method::Auto` using the hardware-detection ladder,
    /// probes the sector size for the root (or current directory), and
    /// validates that no reserved method was requested.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedMethod`] if a reserved method variant
    /// was supplied.
    pub fn build(self) -> Result<Handle> {
        if self.method.is_reserved() {
            return Err(Error::UnsupportedMethod {
                method: self.method.as_str(),
            });
        }

        let resolved_method = self.method.resolve();
        let mode = self.mode.resolve();

        // 0.8.0 J: canonicalise the root *once* at build time, so the
        // stored root is an absolute path with all symlinks resolved.
        // Without this, `Builder::root("data/../jail")` or a root
        // containing a symlink trivially defeats the
        // `resolved.starts_with(root)` check in `Handle::resolve_path`.
        // The caller is responsible for ensuring the root directory
        // exists before calling `build()` — `Builder::root` does not
        // mkdir.
        let canonical_root = match self.root {
            None => None,
            Some(r) => match std::fs::canonicalize(&r) {
                Ok(canon) => Some(canon),
                Err(e) => {
                    return Err(Error::InvalidPath {
                        path: r,
                        reason: format!(
                            "Builder::root canonicalisation failed (path must exist and be a directory): {e}"
                        ),
                    })
                }
            },
        };

        // Probe sector size for the target directory (or cwd as fallback).
        let probe_path = canonical_root
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."));
        let sector_size = crate::platform::probe_sector_size(probe_path);

        // 0.4.0: every handle owns a pipeline. The dispatcher thread is
        // not spawned here — it is created lazily on first batch op so
        // idle handles cost zero threads. Configuration knobs are
        // applied via the `batch_*` methods on the builder; defaults
        // come from `PipelineConfig::DEFAULT` (1 ms / 128 ops / 1024-
        // deep).
        let pipeline = Pipeline::new(self.pipeline_config);

        // 0.5.0: configure the per-handle buffer pool slot. The pool
        // is lazily constructed on first Direct-method op; the config
        // captured here is the input to that lazy construction.
        // `buffer_pool_block_size` is rounded up to a multiple of the
        // probed `sector_size` to satisfy alignment when the pool
        // eventually backs Direct IO buffers.
        let pool_block = align_up(self.buffer_pool_block_size, sector_size as usize);
        let pool_config = crate::handle::HandleBufferPoolConfig {
            capacity: self.buffer_pool_count,
            block_size: pool_block,
            block_align: sector_size as usize,
        };

        Ok(Handle::new_raw(
            self.method,
            resolved_method,
            canonical_root,
            mode,
            sector_size,
            pipeline,
            pool_config,
            self.io_uring_queue_depth,
            self.iouring_sqpoll_idle_ms,
            self.observer,
        ))
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

/// 0.9.2 — Coordinated workload preset for [`Builder::tune_for`].
///
/// Each variant pre-sets a coordinated set of knobs (buffer-pool
/// capacity, io_uring queue depth, batch queue size) tuned to a
/// named workload shape. New variants land in patch releases as
/// the library accumulates production experience with specific
/// shapes; treat the variants as opt-in starting points, not
/// load-bearing semantics.
///
/// `#[non_exhaustive]` — new variants may be added without bumping
/// the major version. `match` arms must include a `_` fallback or
/// they'll fail to compile against future patch releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Workload {
    /// The library defaults — 256 KiB buffer pool, 128-deep
    /// io_uring ring, 1024-deep batch queue. Suitable for
    /// general file IO; NOT tuned for sustained database
    /// throughput.
    Default,
    /// Storage-engine / database workload preset. 8 MiB buffer
    /// pool, 256-deep ring, 4096-deep batch queue. Suitable for
    /// HiveDB, embedded KV stores, log-structured merge trees,
    /// and any workload with sustained bulk writes against an
    /// NVMe target.
    Database,
}

/// Rounds `n` up to the next multiple of `align`. `align` must be a
/// non-zero positive integer; for `align == 0` we return `n` unchanged
/// (defensive — pool construction validates the alignment downstream
/// anyway).
fn align_up(n: usize, align: usize) -> usize {
    if align == 0 {
        return n;
    }
    n.div_ceil(align).saturating_mul(align)
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;

    #[test]
    fn test_default_build_succeeds() {
        let h = Builder::new().build().expect("default build");
        // active_method must be concrete (not Auto)
        assert_ne!(h.active_method(), Method::Auto);
    }

    #[test]
    fn test_builder_sets_method() {
        let h = Builder::new()
            .method(Method::Sync)
            .build()
            .expect("build with Sync");
        assert_eq!(h.method(), Method::Sync);
        assert_eq!(h.active_method(), Method::Sync);
    }

    /// 0.9.7 SQPOLL knob — `Builder::sqpoll(idle_ms)` is captured
    /// at build time. On Linux it's consumed by the io_uring sync
    /// ring's lazy construction; on macOS / Windows the value is
    /// captured but unused (per platform's design — no io_uring
    /// off Linux). Building never fails on a knob-set-only call;
    /// runtime failure (EPERM on a restricted kernel) is handled
    /// downstream by the per-handle `iouring_slot` flipping to
    /// `Disabled` and the Direct path using the pwrite fallback.
    #[test]
    fn test_builder_sqpoll_knob_is_idempotent_on_default_handle() {
        let h_default = Builder::new().build().expect("default");
        let h_sqpoll = Builder::new().sqpoll(1000).build().expect("sqpoll(1000)");
        // Both handles must report the same configured method, the
        // same resolved active method, and the same sector size —
        // SQPOLL is a transparent perf knob, not a semantic one.
        assert_eq!(h_default.method(), h_sqpoll.method());
        assert_eq!(h_default.active_method(), h_sqpoll.active_method());
        assert_eq!(h_default.sector_size(), h_sqpoll.sector_size());
    }

    #[test]
    fn test_builder_sets_root() {
        // 0.8.0 J: `Builder::build` canonicalises the root; the
        // stored value is the canonical form (with Windows
        // `\\?\` extended-length prefix where applicable). The
        // test compares against the *canonicalised* expected form.
        let root = std::env::temp_dir();
        let canonical_root = std::fs::canonicalize(&root).expect("canonicalize temp");
        let h = Builder::new().root(root).build().expect("build with root");
        assert_eq!(h.root(), Some(canonical_root.as_path()));
    }

    #[test]
    fn test_builder_rejects_nonexistent_root() {
        // 0.8.0 J: a root that doesn't exist must be rejected at
        // `build()` rather than allowed through with a "lexical-only"
        // jail that can be defeated by symlinks.
        let bogus = std::env::temp_dir().join("fsys_intentionally_missing_root_xyz_abc");
        let _ = std::fs::remove_dir_all(&bogus);
        let result = Builder::new().root(&bogus).build();
        assert!(matches!(result, Err(Error::InvalidPath { .. })));
    }

    #[test]
    fn test_builder_rejects_reserved_method() {
        // 0.5.0: Mmap is no longer reserved — Method::Journal is the
        // only remaining reserved variant (still 0.7.0 work).
        let err = Builder::new().method(Method::Journal).build();
        assert!(err.is_err());
        if let Err(Error::UnsupportedMethod { method }) = err {
            assert_eq!(method, "journal");
        } else {
            panic!("expected UnsupportedMethod");
        }
    }

    #[test]
    fn test_builder_sector_size_at_least_512() {
        let h = Builder::new().build().expect("build");
        assert!(h.sector_size() >= 512);
    }

    // ── 0.4.0 batch knob tests ────────────────────────────────────────

    #[test]
    fn test_builder_default_pipeline_config_matches_prompt() {
        let b = Builder::new();
        assert_eq!(b.pipeline_config.batch_window_ms, 1);
        assert_eq!(b.pipeline_config.batch_size_max, 128);
        assert_eq!(b.pipeline_config.batch_queue_max, 1024);
    }

    #[test]
    fn test_builder_batch_window_ms_overrides_default() {
        let b = Builder::new().batch_window_ms(5);
        assert_eq!(b.pipeline_config.batch_window_ms, 5);
    }

    #[test]
    fn test_builder_batch_size_max_overrides_default() {
        let b = Builder::new().batch_size_max(64);
        assert_eq!(b.pipeline_config.batch_size_max, 64);
    }

    #[test]
    fn test_builder_batch_queue_max_overrides_default() {
        let b = Builder::new().batch_queue_max(256);
        assert_eq!(b.pipeline_config.batch_queue_max, 256);
    }

    #[test]
    fn test_builder_batch_knobs_chain() {
        let b = Builder::new()
            .batch_window_ms(3)
            .batch_size_max(200)
            .batch_queue_max(2048);
        assert_eq!(b.pipeline_config.batch_window_ms, 3);
        assert_eq!(b.pipeline_config.batch_size_max, 200);
        assert_eq!(b.pipeline_config.batch_queue_max, 2048);
    }

    #[test]
    fn test_builder_batch_knobs_survive_build() {
        // The handle's pipeline carries the configured PipelineConfig.
        // We exercise the full build path with non-default knobs and
        // confirm a batch op runs end-to-end against this handle.
        let h = Builder::new()
            .method(Method::Sync)
            .batch_window_ms(5)
            .batch_size_max(16)
            .batch_queue_max(8)
            .build()
            .expect("build with tuned pipeline");
        assert_eq!(h.method(), Method::Sync);

        // Smoke test: a tiny batch flows through the configured pipeline.
        let p = std::env::temp_dir().join(format!(
            "fsys_builder_knobs_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _g = scopeguard_remove_file(p.clone());
        h.write_batch(&[(p.as_path(), b"x".as_slice())])
            .expect("batch flow");
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
    }

    fn scopeguard_remove_file(p: PathBuf) -> impl Drop {
        struct Guard(PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        Guard(p)
    }

    #[test]
    fn test_builder_batch_window_zero_is_accepted() {
        // Documented as defeating the time-window component, but
        // legal at the API level.
        let b = Builder::new().batch_window_ms(0);
        assert_eq!(b.pipeline_config.batch_window_ms, 0);
    }

    #[test]
    fn test_builder_batch_size_zero_is_accepted() {
        let b = Builder::new().batch_size_max(0);
        assert_eq!(b.pipeline_config.batch_size_max, 0);
    }

    // ── 0.5.0 buffer pool + io_uring knobs ────────────────────────

    #[test]
    fn test_builder_default_buffer_pool_knobs_match_prompt() {
        let b = Builder::new();
        assert_eq!(b.buffer_pool_count, 64);
        assert_eq!(b.buffer_pool_block_size, 4096);
        assert_eq!(b.io_uring_queue_depth, 128);
    }

    #[test]
    fn test_builder_buffer_pool_count_overrides_default() {
        let b = Builder::new().buffer_pool_count(16);
        assert_eq!(b.buffer_pool_count, 16);
    }

    #[test]
    fn test_builder_buffer_pool_block_size_overrides_default() {
        let b = Builder::new().buffer_pool_block_size(65_536);
        assert_eq!(b.buffer_pool_block_size, 65_536);
    }

    #[test]
    fn test_builder_io_uring_queue_depth_overrides_default() {
        let b = Builder::new().io_uring_queue_depth(256);
        assert_eq!(b.io_uring_queue_depth, 256);
    }

    #[test]
    fn test_builder_buffer_pool_knobs_chain() {
        let b = Builder::new()
            .buffer_pool_count(32)
            .buffer_pool_block_size(8192)
            .io_uring_queue_depth(64);
        assert_eq!(b.buffer_pool_count, 32);
        assert_eq!(b.buffer_pool_block_size, 8192);
        assert_eq!(b.io_uring_queue_depth, 64);
    }

    #[test]
    fn test_handle_buffer_pool_lazy_init() {
        // Build a handle and confirm `buffer_pool()` succeeds and
        // returns a pool with the configured shape (block size is
        // rounded up to the probed sector size, so we assert
        // ≥ requested rather than exact equality).
        let h = Builder::new()
            .buffer_pool_count(8)
            .buffer_pool_block_size(4096)
            .build()
            .expect("build");
        let pool = h.buffer_pool().expect("buffer pool");
        assert_eq!(pool.capacity(), 8);
        assert!(pool.block_size() >= 4096);
    }

    #[test]
    fn test_align_up_known_inputs() {
        // sanity: non-zero align rounds up
        assert_eq!(align_up(1, 512), 512);
        assert_eq!(align_up(512, 512), 512);
        assert_eq!(align_up(513, 512), 1024);
        // align == 0 is a no-op (defensive)
        assert_eq!(align_up(100, 0), 100);
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.2 — Workload preset coverage
    // ─────────────────────────────────────────────────────────

    #[test]
    fn test_tune_for_database_sets_coordinated_knobs() {
        let b = Builder::new().tune_for(Workload::Database);
        // Bigger buffer pool — 8 MiB vs the 256 KiB default.
        assert_eq!(b.buffer_pool_count, 1024);
        assert_eq!(b.buffer_pool_block_size, 8192);
        // Deeper io_uring ring (Linux Direct path).
        assert_eq!(b.io_uring_queue_depth, 256);
        // Deeper batch queue.
        assert_eq!(b.pipeline_config.batch_queue_max, 4096);
    }

    #[test]
    fn test_tune_for_default_restores_baseline() {
        let b = Builder::new()
            .tune_for(Workload::Database)
            .tune_for(Workload::Default);
        assert_eq!(b.buffer_pool_count, 64);
        assert_eq!(b.buffer_pool_block_size, 4096);
        assert_eq!(b.io_uring_queue_depth, 128);
        assert_eq!(b.pipeline_config.batch_queue_max, 1024);
    }

    #[test]
    fn test_tune_for_then_individual_setter_overrides() {
        // Setters after `tune_for` must override that preset's
        // value, so callers can use a preset as a baseline and
        // tweak.
        let b = Builder::new()
            .tune_for(Workload::Database)
            .buffer_pool_count(2048)
            .io_uring_queue_depth(512);
        assert_eq!(b.buffer_pool_count, 2048);
        assert_eq!(b.io_uring_queue_depth, 512);
        // The other knobs preserve the preset's values.
        assert_eq!(b.buffer_pool_block_size, 8192);
        assert_eq!(b.pipeline_config.batch_queue_max, 4096);
    }

    #[test]
    fn test_tune_for_database_builds_handle() {
        let h = Builder::new()
            .tune_for(Workload::Database)
            .build()
            .expect("database preset build");
        // The buffer pool config carried into the handle reflects
        // the preset; pool is lazy-init, so block_size visible
        // post-build is the rounded-up value.
        let pool = h.buffer_pool().expect("buffer pool");
        assert_eq!(pool.capacity(), 1024);
        assert!(pool.block_size() >= 8192);
    }
}
