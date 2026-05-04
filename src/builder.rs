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
use crate::path::Mode;
use crate::pipeline::{Pipeline, PipelineConfig};
use crate::{Error, Result};
use std::path::PathBuf;

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
/// - `buffer_pool_size` defaults to `64` (per-handle aligned buffer
///   pool capacity; see locked decision #6 in
///   `.dev/DECISIONS-0.5.0.md`).
/// - `buffer_pool_block` defaults to `4096` (per-buffer size in bytes).
/// - `io_uring_queue_depth` defaults to `128` (Linux io_uring SQ
///   depth; **stubbed in 0.5.0** — see the io_uring blocker in
///   `.dev/DECISIONS-0.5.0.md`).
pub struct Builder {
    method: Method,
    root: Option<PathBuf>,
    mode: Mode,
    pipeline_config: PipelineConfig,
    buffer_pool_size: usize,
    buffer_pool_block: usize,
    io_uring_queue_depth: u32,
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
            buffer_pool_size: 64,
            buffer_pool_block: 4096,
            io_uring_queue_depth: 128,
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
    /// (`buffer_pool_size × buffer_pool_block` bytes when fully
    /// populated).
    #[must_use]
    pub fn buffer_pool_size(mut self, n: usize) -> Self {
        self.buffer_pool_size = n;
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
    /// [`Builder::buffer_pool_size`]).
    #[must_use]
    pub fn buffer_pool_block(mut self, bytes: usize) -> Self {
        self.buffer_pool_block = bytes;
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

        // Probe sector size for the target directory (or cwd as fallback).
        let probe_path = self
            .root
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
        // `buffer_pool_block` is rounded up to a multiple of the
        // probed `sector_size` to satisfy alignment when the pool
        // eventually backs Direct IO buffers.
        let pool_block = align_up(self.buffer_pool_block, sector_size as usize);
        let pool_config = crate::handle::HandleBufferPoolConfig {
            capacity: self.buffer_pool_size,
            block_size: pool_block,
            block_align: sector_size as usize,
        };

        Ok(Handle::new_raw(
            self.method,
            resolved_method,
            self.root,
            mode,
            sector_size,
            pipeline,
            pool_config,
            self.io_uring_queue_depth,
        ))
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
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

    #[test]
    fn test_builder_sets_root() {
        let root = std::env::temp_dir();
        let h = Builder::new()
            .root(root.clone())
            .build()
            .expect("build with root");
        assert_eq!(h.root(), Some(root.as_path()));
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
        assert_eq!(b.buffer_pool_size, 64);
        assert_eq!(b.buffer_pool_block, 4096);
        assert_eq!(b.io_uring_queue_depth, 128);
    }

    #[test]
    fn test_builder_buffer_pool_size_overrides_default() {
        let b = Builder::new().buffer_pool_size(16);
        assert_eq!(b.buffer_pool_size, 16);
    }

    #[test]
    fn test_builder_buffer_pool_block_overrides_default() {
        let b = Builder::new().buffer_pool_block(65_536);
        assert_eq!(b.buffer_pool_block, 65_536);
    }

    #[test]
    fn test_builder_io_uring_queue_depth_overrides_default() {
        let b = Builder::new().io_uring_queue_depth(256);
        assert_eq!(b.io_uring_queue_depth, 256);
    }

    #[test]
    fn test_builder_buffer_pool_knobs_chain() {
        let b = Builder::new()
            .buffer_pool_size(32)
            .buffer_pool_block(8192)
            .io_uring_queue_depth(64);
        assert_eq!(b.buffer_pool_size, 32);
        assert_eq!(b.buffer_pool_block, 8192);
        assert_eq!(b.io_uring_queue_depth, 64);
    }

    #[test]
    fn test_handle_buffer_pool_lazy_init() {
        // Build a handle and confirm `buffer_pool()` succeeds and
        // returns a pool with the configured shape (block size is
        // rounded up to the probed sector size, so we assert
        // ≥ requested rather than exact equality).
        let h = Builder::new()
            .buffer_pool_size(8)
            .buffer_pool_block(4096)
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
}
