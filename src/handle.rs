//! The [`Handle`] struct — the primary entry point for file IO operations.
//!
//! A `Handle` captures the resolved configuration (method, root directory,
//! mode, probed sector size) and provides all CRUD operations through its
//! `impl` blocks defined in [`crate::crud`].
//!
//! `Handle` is `Send + Sync`: the mutable state (active method) is managed
//! with atomic operations. As of `0.4.0`, every `Handle` also owns a
//! pipeline subsystem (crate-internal) that powers the group-lane batch
//! API ([`Handle::write_batch`], [`Handle::delete_batch`],
//! [`Handle::copy_batch`], [`Handle::batch`]). The dispatcher thread is
//! spawned lazily on the first batch submission and shut down cleanly
//! when the `Handle` is dropped — idle handles cost zero threads.

// rustc 1.95 ICE workaround (extension of the 0.5.1 + 0.7.0
// `linux_iouring.rs` / `completion_driver.rs` pattern). The
// `async_iouring_slot: Mutex<AsyncIoUringState>` field references
// `AsyncIoUring`, which transitively touches `io_uring::IoUring`;
// the dead-code analysis pass on this module then ICEs with
// `slice index starts at N but ends at M`. Module-level allow
// skips the buggy lint without affecting correctness — every
// public item here is live by definition (it's the public Handle
// API). Filed as part of the io_uring blocker record in
// `.dev/DECISIONS-0.5.0.md`.
#![allow(dead_code)]

use crate::batch::Batch;
use crate::buffer::AlignedBufferPool;
use crate::error::BatchError;
use crate::method::Method;
use crate::path::Mode;
use crate::pipeline::{BatchOp, HandleSnapshot, Pipeline};
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicU8, Ordering};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
use crate::platform::linux_iouring::{IoUringRing, NvmeAccess};
#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(all(target_os = "linux", feature = "async"))]
use crate::async_io::completion_driver::AsyncIoUring;

#[cfg(target_os = "windows")]
use crate::platform::windows_nvme::NvmeAccess as WinNvmeAccess;
#[cfg(target_os = "windows")]
use std::sync::Arc as WinArc;

/// A capability probed once per handle, for the first storage device a
/// Direct op touches.
///
/// NVMe passthrough flushes a specific controller / volume. Before
/// 1.1.1 the handle cached the probe result from the first file's
/// device and then used it for every later file, so writes to another
/// device were "flushed" on the wrong one. The cache now remembers the
/// device key (`st_dev` on Linux, the volume serial number on Windows)
/// it was probed for; ops on any other device get `None` and use the
/// standard fence for their own file instead.
///
/// After the first probe, lookups are a single atomic load (no lock).
pub(crate) struct DeviceKeyed<T> {
    slot: std::sync::OnceLock<(u64, Option<std::sync::Arc<T>>)>,
}

impl<T> DeviceKeyed<T> {
    pub(crate) const fn new() -> Self {
        Self {
            slot: std::sync::OnceLock::new(),
        }
    }

    /// Returns the capability for the device identified by `key`,
    /// running `probe` the first time any device is seen. `None` when
    /// the probe failed or `key` is not the probed device.
    pub(crate) fn get(
        &self,
        key: u64,
        probe: impl FnOnce() -> Option<T>,
    ) -> Option<std::sync::Arc<T>> {
        let (probed_key, value) = self
            .slot
            .get_or_init(|| (key, probe().map(std::sync::Arc::new)));
        if *probed_key == key {
            value.clone()
        } else {
            None
        }
    }

    /// `true` once a probe has succeeded for some device. Does not
    /// probe.
    pub(crate) fn is_active(&self) -> bool {
        matches!(self.slot.get(), Some((_, Some(_))))
    }
}

/// Pool configuration captured by [`Builder`] and consumed at
/// [`Handle`] construction. Held opaquely in the Handle until the
/// first Direct-method op triggers lazy pool allocation (locked
/// decision #6 in `.dev/DECISIONS-0.5.0.md`).
#[derive(Clone, Copy)]
pub(crate) struct HandleBufferPoolConfig {
    pub capacity: usize,
    pub block_size: usize,
    pub block_align: usize,
}

// ──────────────────────────────────────────────────────────────────────────────
// Temp-file naming
// ──────────────────────────────────────────────────────────────────────────────

/// Process-global counter mixed into every temp-file nonce so two
/// writes from the same process never derive the same name, even when
/// the clock does not advance between them.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Longest single path component accepted by the common filesystems
/// (ext4, XFS, btrfs, APFS and NTFS all cap a name at 255 bytes or
/// UTF-16 units).
const MAX_NAME_LEN: usize = 255;

/// Prefix shared by every temp file fsys creates. Stable so recovery
/// tooling can find orphans left behind by a crash.
const TEMP_PREFIX: &str = ".fsys-tmp-";

// ──────────────────────────────────────────────────────────────────────────────

/// The primary entry point for all fsys file IO operations.
///
/// A `Handle` holds the resolved configuration for a single IO context:
/// durability method, root directory scope, operating mode, and probed
/// sector size. All CRUD methods are implemented as `impl Handle` blocks in
/// the [`crate::crud`] module.
///
/// # Thread safety
///
/// `Handle` is `Send + Sync`. The [`active_method`](Handle::active_method)
/// field is managed with atomic operations so multiple threads can share a
/// single `Handle` without additional locking.
///
/// # Building a Handle
///
/// Use [`crate::builder()`] (preferred) or [`crate::new()`] for a
/// zero-configuration default:
///
/// ```
/// # fn example() -> fsys::Result<()> {
/// let handle = fsys::builder()
///     .method(fsys::Method::Auto)
///     .build()?;
/// # Ok(())
/// # }
/// ```
pub struct Handle {
    /// The method explicitly requested by the caller (possibly `Auto`).
    configured_method: AtomicU8,
    /// The method currently in effect after runtime fallbacks.
    ///
    /// Set to the resolved form of `configured_method` at build time.
    /// May be updated to a less-capable method if the OS rejects a
    /// privileged open (e.g. `O_DIRECT` rejected on tmpfs → falls back
    /// to `Data`).
    ///
    /// **0.4.0 limitation.** This field is updated by solo-lane runtime
    /// fallbacks but **not** by group-lane (batch) per-op fallbacks —
    /// the dispatcher runs without a [`Handle`] reference. Group-lane
    /// fallback information surfaces in [`BatchError::source`] for the
    /// failing op. See decision D-5 in `.dev/DECISIONS-0.4.0.md`; full
    /// cross-lane consistency arrives in `0.5.0`.
    active_method: AtomicU8,
    /// Optional root directory. When set, all relative paths are resolved
    /// against this root and path-escape checks are enforced.
    root: Option<PathBuf>,
    /// Operating mode — affects default path selection.
    mode: Mode,
    /// Probed logical sector size for aligned Direct IO buffers (bytes).
    sector_size: u32,
    /// Per-handle pipeline. Owns the lazy group-lane dispatcher thread.
    /// Declared last so its `Drop` runs after the rest of the state has
    /// already been read into snapshots — although correctness does not
    /// depend on field-drop order (the dispatcher consumes only its
    /// `BatchJob`-supplied [`HandleSnapshot`]s, never the live state).
    pipeline: Pipeline,
    /// Buffer pool config (capacity, block size, alignment). Captured
    /// at construction and used by [`Handle::buffer_pool`] for lazy
    /// allocation.
    pool_config: HandleBufferPoolConfig,
    /// Lazy aligned buffer pool. `None` until the first Direct-method
    /// op leases a buffer; `Some(...)` for the rest of this Handle's
    /// lifetime. The Mutex is held only briefly during lazy init —
    /// once the pool is constructed, leasing is lock-free on the
    /// fast path.
    /// Lock-free slot — `OnceLock::get()` is a single atomic load
    /// after first init, so the buffer-pool fast path on every
    /// Direct write costs zero mutex acquires. The slot is set
    /// exactly once (lazy init); after that, all reads are
    /// uncontended atomic loads. (0.8.0 I round-3 perf fix —
    /// previously `Mutex<Option<AlignedBufferPool>>` cost a mutex
    /// acquire per Direct op even after init.)
    pool_slot: std::sync::OnceLock<AlignedBufferPool>,
    /// Linux-only: requested `io_uring` SQ depth (from
    /// [`crate::Builder::io_uring_queue_depth`]). Captured at
    /// construction; consumed by [`Handle::io_uring_ring`] on the
    /// first Direct-method op.
    #[cfg(target_os = "linux")]
    iouring_queue_depth: u32,
    /// Linux-only: opt-in `IORING_SETUP_SQPOLL` idle timeout in
    /// milliseconds, from [`crate::Builder::sqpoll`]. `None` =
    /// SQPOLL disabled (default; no kernel polling thread).
    /// `Some(idle_ms)` = enable SQPOLL with the given idle
    /// timeout. On kernels / environments that reject the setup
    /// (EPERM on < 5.13 without CAP_SYS_NICE, restricted
    /// sandboxes), `IoUringRing::new` returns the setup error
    /// and `iouring_slot` caches `None` — the Direct path then
    /// falls back to the `pwrite` path cleanly.
    #[cfg(target_os = "linux")]
    iouring_sqpoll_idle_ms: Option<u32>,
    /// Linux-only: lazy `io_uring` ring. Empty until the first Direct
    /// op; then `Some(ring)`, or `None` when construction failed (kernel
    /// < 5.1, seccomp, container restriction), for the rest of this
    /// Handle's lifetime. After initialisation every lookup is a single
    /// atomic load; earlier versions took a mutex on every Direct op.
    #[cfg(target_os = "linux")]
    iouring_slot: OnceLock<Option<Arc<IoUringRing>>>,
    /// Linux-only: NVMe-passthrough capability (an owned `/dev/nvmeX`
    /// handle plus namespace id), probed on the first Direct op and
    /// keyed by the `st_dev` it was probed for.
    #[cfg(target_os = "linux")]
    nvme_slot: DeviceKeyed<NvmeAccess>,
    /// Windows-only: NVMe-passthrough capability (the resolved volume
    /// root; volume handles are reopened per op), keyed by the volume
    /// serial number it was probed for.
    #[cfg(target_os = "windows")]
    nvme_slot_win: DeviceKeyed<WinNvmeAccess>,
    /// Linux + `async` feature only: lazy native io_uring async
    /// substrate. Same states as `iouring_slot`. New in `0.7.0`.
    #[cfg(all(target_os = "linux", feature = "async"))]
    async_iouring_slot: OnceLock<Option<Arc<AsyncIoUring>>>,
    /// 0.9.2: optional structured-telemetry observer. Registered
    /// once at handle-construction time via
    /// [`crate::Builder::observer`]; cloned (cheap `Arc::clone`)
    /// into every [`crate::JournalHandle`] this handle opens, so
    /// journal-side hot paths can fire events directly without
    /// borrowing back into the handle. `None` for handles built
    /// without an observer — the per-op cost is then a single
    /// `Option::is_some` branch on the caller's thread.
    pub(crate) observer: Option<std::sync::Arc<dyn crate::observer::FsysObserver>>,
}

impl Handle {
    /// Creates a `Handle` from raw components.
    ///
    /// This is `pub(crate)` — external callers use [`crate::Builder`].
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    #[allow(clippy::too_many_arguments)] // every arg is load-bearing handle state — splitting would obscure the struct shape
    pub(crate) fn new_raw(
        configured_method: Method,
        active_method: Method,
        root: Option<PathBuf>,
        mode: Mode,
        sector_size: u32,
        pipeline: Pipeline,
        pool_config: HandleBufferPoolConfig,
        iouring_queue_depth: u32,
        iouring_sqpoll_idle_ms: Option<u32>,
        observer: Option<std::sync::Arc<dyn crate::observer::FsysObserver>>,
    ) -> Self {
        Self {
            configured_method: AtomicU8::new(configured_method.to_u8()),
            active_method: AtomicU8::new(active_method.to_u8()),
            root,
            mode,
            sector_size,
            pipeline,
            pool_config,
            pool_slot: std::sync::OnceLock::new(),
            #[cfg(target_os = "linux")]
            iouring_queue_depth,
            #[cfg(target_os = "linux")]
            iouring_sqpoll_idle_ms,
            #[cfg(target_os = "linux")]
            iouring_slot: OnceLock::new(),
            #[cfg(target_os = "linux")]
            nvme_slot: DeviceKeyed::new(),
            #[cfg(target_os = "windows")]
            nvme_slot_win: DeviceKeyed::new(),
            #[cfg(all(target_os = "linux", feature = "async"))]
            async_iouring_slot: OnceLock::new(),
            observer,
        }
    }

    /// 0.9.2: returns the [`crate::observer::FsysObserver`] this
    /// handle was built with, if any. Use to confirm observer
    /// registration in tests; the hot paths consult this slot
    /// internally without going through the public method.
    #[must_use]
    #[inline]
    pub fn observer(&self) -> Option<&std::sync::Arc<dyn crate::observer::FsysObserver>> {
        self.observer.as_ref()
    }

    /// Returns the per-handle native async io_uring substrate,
    /// constructing it on first call. Cached `None` after a
    /// construction failure so subsequent native-substrate
    /// submissions don't retry the syscall every op.
    ///
    /// Linux + `async` feature only. Must be called from inside a
    /// tokio runtime context (the constructor spawns the
    /// completion-driver task on the current runtime).
    #[cfg(all(target_os = "linux", feature = "async"))]
    pub(crate) fn async_io_uring(&self) -> Option<Arc<AsyncIoUring>> {
        self.async_iouring_slot
            .get_or_init(|| {
                AsyncIoUring::new(self.iouring_queue_depth)
                    .ok()
                    .map(Arc::new)
            })
            .clone()
    }

    /// Returns the Windows NVMe-passthrough access for the volume that
    /// holds `file` (whose path is `path`), probing on the first call.
    /// `None` when the probe failed or `file` lives on a different
    /// volume than the one probed.
    #[cfg(target_os = "windows")]
    pub(crate) fn nvme_access_win(
        &self,
        file: &std::fs::File,
        path: &Path,
    ) -> Option<WinArc<WinNvmeAccess>> {
        let serial = volume_serial(file)?;
        self.nvme_slot_win.get(serial, || {
            crate::platform::windows_nvme::nvme_flush_capable(path)
        })
    }

    /// Returns the NVMe passthrough access for the block device that
    /// holds `file`, probing on the first call. The probe resolves the
    /// fd to `/dev/nvmeX` and verifies privilege. `None` when the probe
    /// failed or `file` lives on a different device than the one
    /// probed.
    #[cfg(target_os = "linux")]
    pub(crate) fn nvme_access(&self, file: &std::fs::File) -> Option<Arc<NvmeAccess>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let dev = file.metadata().ok()?.dev();
        self.nvme_slot.get(dev, || {
            crate::platform::linux_iouring::nvme_flush_capable(file.as_raw_fd())
        })
    }

    /// Returns the canonical name of the durability primitive this
    /// handle currently invokes for write durability. The exact
    /// strings are defined as constants in [`crate::primitive`] —
    /// match against those rather than the raw string to avoid
    /// typos.
    ///
    /// The value reflects the **resolved** primitive after lazy
    /// probes (io_uring construction, NVMe passthrough capability
    /// detection, mmap suitability) — not the configured method.
    /// Probes happen on the first IO op; before that, this returns
    /// the conservative-fallback primitive for the configured
    /// method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::{builder, primitive};
    ///
    /// let fs = builder().build()?;
    /// match fs.active_durability_primitive() {
    ///     primitive::IO_URING_NVME_FLUSH => println!("elite path"),
    ///     primitive::IO_URING_FDATASYNC => println!("standard io_uring"),
    ///     primitive::FSYNC => println!("fallback fsync"),
    ///     _ => println!("other"),
    /// }
    /// # Ok::<(), fsys::Error>(())
    /// ```
    #[must_use]
    pub fn active_durability_primitive(&self) -> &'static str {
        let method = self.active_method();
        match method {
            Method::Mmap => crate::primitive::MMAP_MSYNC,
            Method::Sync => {
                #[cfg(target_os = "macos")]
                {
                    crate::primitive::F_FULLFSYNC
                }
                #[cfg(not(target_os = "macos"))]
                {
                    crate::primitive::FSYNC
                }
            }
            Method::Data => {
                #[cfg(target_os = "linux")]
                {
                    crate::primitive::FDATASYNC
                }
                #[cfg(target_os = "macos")]
                {
                    crate::primitive::F_FULLFSYNC
                }
                #[cfg(target_os = "windows")]
                {
                    crate::primitive::FSYNC
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                {
                    crate::primitive::FSYNC
                }
            }
            Method::Direct => {
                #[cfg(target_os = "linux")]
                {
                    self.linux_direct_primitive()
                }
                #[cfg(target_os = "macos")]
                {
                    crate::primitive::F_NOCACHE_F_FULLFSYNC
                }
                #[cfg(target_os = "windows")]
                {
                    self.windows_direct_primitive()
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                {
                    crate::primitive::FSYNC
                }
            }
            // Reserved / unreachable variants — return a conservative
            // fallback rather than panicking. `Method::Auto` is
            // resolved to a concrete method at handle construction,
            // so it should not be observed here in practice.
            _ => crate::primitive::FSYNC,
        }
    }

    /// Resolves the active Direct primitive on Linux based on the
    /// cached io_uring + NVMe slot state. Pure read of the cached
    /// state — does NOT trigger probing (probing is driven by IO ops
    /// in `crud/file.rs`).
    #[cfg(target_os = "linux")]
    fn linux_direct_primitive(&self) -> &'static str {
        if self.nvme_slot.is_active() {
            return crate::primitive::IO_URING_NVME_FLUSH;
        }
        if matches!(self.iouring_slot.get(), Some(Some(_))) {
            crate::primitive::IO_URING_FDATASYNC
        } else {
            crate::primitive::O_DIRECT_PWRITE_FDATASYNC
        }
    }

    /// Resolves the active Direct primitive on Windows based on the
    /// cached NVMe slot state. Pure read of the cached state — does
    /// NOT trigger probing.
    #[cfg(target_os = "windows")]
    fn windows_direct_primitive(&self) -> &'static str {
        if self.nvme_slot_win.is_active() {
            crate::primitive::FILE_FLAG_WRITE_THROUGH_NVME_IOCTL
        } else {
            crate::primitive::FILE_FLAG_WRITE_THROUGH
        }
    }

    /// Returns which async runtime substrate this handle currently
    /// uses. New in `0.7.0`.
    ///
    /// The substrate is computed on each call (no probing — pure
    /// read of cached state). It transitions from
    /// [`crate::AsyncSubstrate::SpawnBlocking`] to
    /// [`crate::AsyncSubstrate::NativeIoUring`] automatically when the
    /// per-handle io_uring ring is lazily constructed (typically on
    /// the first [`Method::Direct`] op).
    ///
    /// On non-Linux platforms, on Linux without the `async` Cargo
    /// feature, when `Method::Direct` is not active, when the
    /// io_uring ring failed to construct, or when
    /// `FSYS_DISABLE_NATIVE_ASYNC=1` is set, this returns
    /// [`crate::AsyncSubstrate::SpawnBlocking`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::AsyncSubstrate;
    ///
    /// # fn example() -> fsys::Result<()> {
    /// let fs = fsys::builder().method(fsys::Method::Direct).build()?;
    /// match fs.async_substrate() {
    ///     AsyncSubstrate::NativeIoUring => println!("native fast path"),
    ///     AsyncSubstrate::SpawnBlocking => println!("portable fallback"),
    ///     // `AsyncSubstrate` is `#[non_exhaustive]`.
    ///     _ => unreachable!(),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn async_substrate(&self) -> crate::AsyncSubstrate {
        if self.substrate_is_native() {
            crate::AsyncSubstrate::NativeIoUring
        } else {
            crate::AsyncSubstrate::SpawnBlocking
        }
    }

    /// Linux + `async` feature substrate-selection check. Pure
    /// read of cached state; does NOT trigger probe construction.
    /// On Linux without the `async` feature (or non-Linux), this
    /// always returns `false` — the native substrate is unreachable.
    #[cfg(all(target_os = "linux", feature = "async"))]
    fn substrate_is_native(&self) -> bool {
        if native_async_disabled_by_env() {
            return false;
        }
        if self.active_method() != Method::Direct {
            return false;
        }
        // Only report native when the ASYNC ring is constructed and
        // its driver isn't poisoned. The first async Direct op finds
        // SpawnBlocking (async ring not yet constructed), routes
        // through spawn_blocking; the next op finds the cached
        // result. A panicked driver is functionally fallback even if
        // the ring exists.
        matches!(self.async_iouring_slot.get(), Some(Some(ring)) if !ring.is_poisoned())
    }

    /// Linux without async feature: native substrate is gated by
    /// the feature, never reachable.
    #[cfg(all(target_os = "linux", not(feature = "async")))]
    fn substrate_is_native(&self) -> bool {
        false
    }

    /// Non-Linux: native substrate is never available.
    #[cfg(not(target_os = "linux"))]
    fn substrate_is_native(&self) -> bool {
        false
    }

    /// Returns the per-handle io_uring ring, constructing it on the
    /// first call. Cached `None` after a construction failure so
    /// subsequent Direct ops don't retry the syscall.
    ///
    /// Linux only. On every other platform the analogous code path
    /// in `crud/file.rs` is `#[cfg]`-gated and never calls this
    /// method.
    #[cfg(target_os = "linux")]
    pub(crate) fn io_uring_ring(&self) -> Option<Arc<IoUringRing>> {
        self.iouring_slot
            .get_or_init(|| {
                IoUringRing::new(self.iouring_queue_depth, self.iouring_sqpoll_idle_ms)
                    .ok()
                    .map(Arc::new)
            })
            .clone()
    }

    /// Test-only: marks the `io_uring` ring as unavailable so Direct
    /// ops take the platform `pwrite` path, the same path a kernel
    /// without `io_uring` (or a restricted container) would take. Must
    /// run before the handle's first Direct op.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn disable_io_uring_for_test(&self) {
        let _ = self.iouring_slot.set(None);
    }

    /// Returns a clone of the per-handle aligned buffer pool,
    /// allocating it on first call.
    ///
    /// The pool itself is `Arc<PoolInner>`-cloned cheaply; the
    /// underlying allocations are shared across all clones. Idle
    /// handles cost zero buffer memory beyond the `Mutex<Option<…>>`
    /// slot until this method is called.
    ///
    /// Returns the pool's lazy-construction error
    /// ([`Error::AlignmentRequired`]) when the configured
    /// `buffer_pool_count`/`buffer_pool_block_size` is invalid against the
    /// probed sector size.
    #[allow(dead_code)] // wired into Direct path in 0.5.x patch alongside io_uring lift
    pub(crate) fn buffer_pool(&self) -> Result<AlignedBufferPool> {
        // Fast path: post-init read is a single atomic load + Arc clone.
        if let Some(pool) = self.pool_slot.get() {
            return Ok(pool.clone());
        }
        // Slow path: first-init. Construct, then race-set into the
        // OnceLock. If we lose the race (another thread populated
        // the slot first), our local `pool` is dropped and we
        // return the slot's value. Either way, the slot is
        // populated exactly once for this handle's lifetime.
        let pool = AlignedBufferPool::new(
            self.pool_config.capacity,
            self.pool_config.block_size,
            self.pool_config.block_align,
        )?;
        // `set` returns Err with our `pool` if the slot was already
        // populated by a racing thread. Either way, after `set`
        // returns the slot is definitely populated — by us or by
        // the winner. We always read from the slot so callers from
        // different threads see a coherent pool (lease/return
        // pairing requires the same allocation).
        let _ = self.pool_slot.set(pool);
        self.pool_slot.get().cloned().ok_or_else(|| {
            Error::Io(std::io::Error::other(
                "buffer pool slot was unset after set — impossible",
            ))
        })
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Public accessors
    // ──────────────────────────────────────────────────────────────────────────

    /// Returns the method that was configured by the caller.
    ///
    /// This may be [`Method::Auto`] if the caller did not specify a method;
    /// see [`Handle::active_method`] for the resolved value.
    #[must_use]
    #[inline]
    pub fn method(&self) -> Method {
        Method::from_u8(self.configured_method.load(Ordering::Relaxed))
    }

    /// Returns the method currently in effect after any runtime fallbacks.
    ///
    /// This is always a concrete method (`Sync`, `Data`, or `Direct`) —
    /// never `Auto`. If `O_DIRECT` was rejected at open time and the
    /// handle fell back to `Data`, this method will reflect that change.
    #[must_use]
    #[inline]
    pub fn active_method(&self) -> Method {
        Method::from_u8(self.active_method.load(Ordering::Relaxed))
    }

    /// Updates the configured method for future IO operations.
    ///
    /// Resolves [`Method::Auto`] through the hardware-probe ladder
    /// (same logic as [`Builder::build`](crate::Builder::build)) and
    /// publishes both the configured + resolved values atomically.
    /// Existing in-flight IO is unaffected; only subsequent calls
    /// pick up the new method.
    ///
    /// # Errors
    ///
    /// - [`Error::UnsupportedMethod`] if `method` is a reserved
    ///   variant ([`Method::Journal`] — see its docs for why it's
    ///   reserved).
    pub fn set_method(&self, method: Method) -> Result<()> {
        if method.is_reserved() {
            return Err(Error::UnsupportedMethod {
                method: method.as_str(),
            });
        }
        let resolved = method.resolve();
        self.configured_method
            .store(method.to_u8(), Ordering::Relaxed);
        self.active_method
            .store(resolved.to_u8(), Ordering::Relaxed);
        Ok(())
    }

    /// Returns the root directory scope, if one was configured via
    /// [`Builder::root`](crate::Builder::root).
    ///
    /// When set, every path passed to a `Handle::*` method is
    /// resolved against this root, and absolute paths that escape it
    /// are rejected with [`Error::InvalidPath`]. The returned path
    /// is canonical (`build()` runs `std::fs::canonicalize` once).
    #[must_use]
    #[inline]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Returns the resolved operating mode ([`Mode::Dev`] or
    /// [`Mode::Prod`]; [`Mode::Auto`] is resolved at build time).
    ///
    /// Affects defaults for path selection inside [`fsys::path`](crate::path)
    /// helpers.
    #[must_use]
    #[inline]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Returns the probed logical sector size in bytes.
    ///
    /// Captured once at handle construction via the platform's
    /// sector-size probe (Linux `ioctl(BLKSSZGET)`, macOS
    /// `IOServiceGetMatchingService`, Windows
    /// `STORAGE_PROPERTY_QUERY`). Used to size aligned [`Method::Direct`]
    /// IO buffers and to round up `buffer_pool_block_size` to a sector
    /// multiple.
    ///
    /// Typical values are 512 (legacy disks, 512e SSDs) or 4096
    /// (most modern NVMe / 4Kn drives).
    #[must_use]
    #[inline]
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    // ──────────────────────────────────────────────────────────────────────────
    // 0.9.2 — Hardware-aware database decision surface
    // ──────────────────────────────────────────────────────────────────────────

    /// 0.9.2 — Returns `true` if the storage device is **confirmed**
    /// to provide power-loss protection (PLP).
    ///
    /// PLP — typically a tantalum or supercapacitor onboard the
    /// drive — guarantees that any data the host has handed to the
    /// drive's write cache (whether or not `fsync` has been called)
    /// will reach the NAND on power loss. Enterprise NVMe and
    /// SAS SSDs commonly include PLP; consumer SSDs almost never
    /// do.
    ///
    /// **What this enables.** For a PLP-protected drive, durable
    /// writes need only the `pwrite` syscall to reach the drive's
    /// write cache — `fsync` / `fdatasync` becomes a strict no-op
    /// from a crash-safety perspective. A database aware of this
    /// can skip the per-commit fsync and still meet its durability
    /// contract, multiplying transaction throughput on the order
    /// of 3–10× on enterprise hardware. Oracle Exadata / SQL
    /// Server's "Persistent Memory" tiers exploit exactly this
    /// signal.
    ///
    /// **Conservative semantics.** This method returns `true` ONLY
    /// when the probe confirmed PLP via the vendor allowlist or
    /// (on Linux) the NVMe Volatile-Write-Cache bit. It returns
    /// `false` for both confirmed-no and unknown — never lie that
    /// durability is guaranteed when we don't know. Callers
    /// considering an fsync-skip optimisation should treat `false`
    /// as "must fsync" without hesitation. See [`Self::plp_status`]
    /// for the underlying tri-state.
    ///
    /// **Not free.** PLP detection is per-process probing, cached
    /// for the process lifetime. A hot-plugged drive that arrives
    /// after fsys's first probe is not re-detected.
    #[must_use]
    pub fn is_plp_protected(&self) -> bool {
        matches!(
            crate::hardware::drive().plp,
            crate::hardware::PlpStatus::Yes
        )
    }

    /// 0.9.5 — Punches a hole in `path` at `[offset, offset + len)`.
    ///
    /// After this call the file's logical size is **unchanged** —
    /// the byte range is still addressable and reads return
    /// zeros — but the underlying storage blocks are released
    /// to the filesystem free pool. Most modern filesystems
    /// (ext4 with `discard`, xfs, btrfs, APFS, NTFS-sparse)
    /// also notify the underlying NVMe / SATA SSD via TRIM /
    /// DEALLOCATE, so the drive's wear-leveler can reuse the
    /// erase blocks.
    ///
    /// **Use cases.** WAL truncation (free the prefix of a
    /// journal after a checkpoint), sparse-file management
    /// (release a dead range from a key-value store's data
    /// file), database vacuum operations.
    ///
    /// **Per-platform implementation:**
    /// - **Linux**: `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`.
    ///   On filesystems with `discard` mount option, the kernel
    ///   issues NVMe DEALLOCATE / SATA TRIM automatically.
    /// - **macOS**: `fcntl(F_PUNCHHOLE)` (10.12+).
    /// - **Windows**: `DeviceIoControl(FSCTL_SET_ZERO_DATA)` —
    ///   on NTFS sparse files this truly releases blocks; on
    ///   regular files it zero-fills (same observable
    ///   semantics — reads return zeros).
    /// - **Other**: returns `Error::Io` with `Unsupported` kind.
    ///
    /// **Sector alignment.** Most filesystems will silently
    /// round the range to filesystem-block boundaries (4 KiB
    /// typical). Callers that need precise byte-level zeros
    /// should follow with [`Self::write_zeros`] on the
    /// trailing partial sectors.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if path resolution fails.
    /// - [`Error::Io`] wrapping the underlying syscall error.
    ///   Common variants: `EOPNOTSUPP` on filesystems that
    ///   don't support hole-punching (older ext2, vfat,
    ///   certain FUSE mounts) — caller's data is unchanged.
    pub fn punch_hole(
        &self,
        path: impl AsRef<std::path::Path>,
        offset: u64,
        len: u64,
    ) -> Result<()> {
        let resolved = self.resolve_path(path.as_ref())?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&resolved)
            .map_err(Error::Io)?;
        crate::platform::punch_hole(&file, offset, len)
    }

    /// 0.9.5 — Zero-fills `path` at `[offset, offset + len)`.
    ///
    /// On capable Linux + NVMe configurations the kernel
    /// translates this into an NVMe `WRITE ZEROES` command —
    /// the drive controller marks the range as zeros without
    /// any host→device data transfer. On other platforms /
    /// configurations the implementation falls back to a
    /// regular `pwrite` of an aligned zero buffer.
    ///
    /// Unlike [`Self::punch_hole`], `write_zeros` **does not**
    /// release the underlying storage — the bytes are
    /// guaranteed to read as zeros, but the blocks remain
    /// allocated. Use this when you need explicit byte-level
    /// zero semantics without changing the file's storage
    /// footprint (e.g., pre-zeroing a WAL segment to avoid
    /// sparse-file metadata bookkeeping during sustained
    /// appends).
    ///
    /// **Per-platform implementation:**
    /// - **Linux**: `fallocate(FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE)`.
    /// - **macOS / Windows / other**: aligned-buffer `pwrite`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if path resolution fails.
    /// - [`Error::Io`] wrapping the underlying syscall error.
    pub fn write_zeros(
        &self,
        path: impl AsRef<std::path::Path>,
        offset: u64,
        len: u64,
    ) -> Result<()> {
        let resolved = self.resolve_path(path.as_ref())?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&resolved)
            .map_err(Error::Io)?;
        crate::platform::zero_range(&file, offset, len)
    }

    /// 0.9.4 — Returns the device's **atomic-write unit** (NAWUPF)
    /// in **bytes**, or `None` when the probe could not determine
    /// it.
    ///
    /// **NAWUPF** is the NVMe "Namespace Atomic Write Unit Power
    /// Fail" — the largest write size the device guarantees will
    /// be atomically committed across a power-fail event.
    /// Databases aware of it can **skip torn-write detection** on
    /// writes up to this size (a hot-path optimisation for
    /// write-heavy workloads on enterprise NVMe: torn-write
    /// detection typically costs an extra checksum + a per-write
    /// branch).
    ///
    /// **The conversion.** NAWUPF is reported by the device as a
    /// 0-based count of logical blocks: `NAWUPF = N` means
    /// `(N + 1) × logical_sector` bytes are atomically committed.
    /// This method returns the byte count directly, so callers
    /// don't need to know the logical sector size.
    ///
    /// **Conservative semantics, same as
    /// [`Self::is_plp_protected`].** Returns `None` whenever
    /// fsys cannot confirm the guarantee — non-NVMe drive,
    /// privilege denied on `/dev/nvmeX`, NVMe sentinel `0xFFFF`
    /// (unsupported), or non-Linux platform (the probe currently
    /// lives in the Linux platform layer; future patches may add
    /// Windows / macOS NVMe-Identify-Namespace paths). Callers
    /// MUST treat `None` as "no atomic guarantee — protect every
    /// write".
    ///
    /// **Probing happens once at handle creation** (via
    /// [`crate::hardware::info`]) and the result is cached for
    /// the lifetime of the process. Hot-plug is not re-probed.
    #[must_use]
    pub fn atomic_write_unit(&self) -> Option<u32> {
        let drive = crate::hardware::drive();
        let n_lba = drive.nawupf_lba?;
        // NAWUPF is 0-based per the NVMe spec; the device
        // guarantees (N + 1) logical blocks atomically. Use
        // checked arithmetic — paranoid against pathological
        // u32 wraparound on values near u32::MAX (which only
        // shows up if the parser ever skipped the 0xFFFF
        // sentinel check, which it doesn't).
        let blocks = n_lba.checked_add(1)?;
        blocks.checked_mul(drive.logical_sector)
    }

    /// 0.9.2 — Returns the underlying [`crate::hardware::PlpStatus`]
    /// tri-state (`Yes` / `No` / `Unknown`).
    ///
    /// Use this when a `bool` is too coarse — for instance, when a
    /// callers wants to log "drive PLP unknown, falling back to
    /// fdatasync" vs "drive confirmed no PLP, fdatasync mandatory".
    /// See [`Self::is_plp_protected`] for the most common case.
    #[must_use]
    pub fn plp_status(&self) -> crate::hardware::PlpStatus {
        crate::hardware::drive().plp
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Journal API (0.8.0)
    //
    // High-throughput append-only durability primitive. Independent of
    // [`Method`] — works with every Handle regardless of how the parent
    // Handle was constructed. See [`crate::journal`] for the design
    // rationale and the `JournalHandle` API.
    // ──────────────────────────────────────────────────────────────────────────

    /// Opens an append-only journal at `path`.
    ///
    /// The journal is the high-throughput durability primitive
    /// that databases / queues / ledgers should use for WAL-style
    /// workloads. Unlike [`Handle::write`] (atomic-replace, 5–7
    /// syscalls per call, fsync per call), the journal opens
    /// once, supports concurrent appends without per-call fsync,
    /// and exposes group-commit durability via
    /// [`crate::JournalHandle::sync_through`].
    ///
    /// If `path` already exists, the journal resumes at the
    /// existing file size (next LSN = existing length). If not,
    /// the file is created.
    ///
    /// `path` is resolved against the handle's
    /// [`crate::Builder::root`] scope if one is configured, with
    /// the same canonical-prefix security check as
    /// [`Handle::write`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use fsys::builder;
    ///
    /// # fn main() -> fsys::Result<()> {
    /// let fs = builder().build()?;
    /// let log = Arc::new(fs.journal("/var/log/app.wal")?);
    ///
    /// // Many appends, no fsync.
    /// let _lsn1 = log.append(b"event 1")?;
    /// let _lsn2 = log.append(b"event 2")?;
    /// let lsn3 = log.append(b"event 3")?;
    ///
    /// // One group-commit fsync covers all three.
    /// log.sync_through(lsn3)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on the underlying open failure.
    pub fn journal(&self, path: impl AsRef<std::path::Path>) -> Result<crate::JournalHandle> {
        let resolved = self.resolve_path(path.as_ref())?;
        let mut journal = crate::journal::JournalHandle::open(&resolved)?;
        journal.set_observer(self.observer.clone());
        Ok(journal)
    }

    /// Opens an append-only journal at `path` honoring the
    /// supplied [`crate::JournalOptions`].
    ///
    /// Use this entry point when you need Direct-IO mode
    /// (`JournalOptions::direct(true)`) or a non-default log
    /// buffer size. For the standard buffered-mode path, use
    /// [`Self::journal`].
    ///
    /// Path resolution is identical to [`Self::journal`] — the
    /// path is canonicalised against the handle root if one is
    /// configured, with the same security check.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use fsys::{builder, JournalOptions};
    ///
    /// # fn main() -> fsys::Result<()> {
    /// let fs = builder().build()?;
    /// let log = Arc::new(fs.journal_with(
    ///     "/var/lib/mydb/wal",
    ///     JournalOptions::new().direct(true).log_buffer_kib(256),
    /// )?);
    ///
    /// // Same API as a standard journal — direct mode is
    /// // transparent to the caller.
    /// let _lsn = log.append(b"event 1")?;
    /// log.sync_through(log.next_lsn())?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on the underlying open failure or — in
    ///   direct mode — on a non-recoverable resume tail state
    ///   (`BadMagic`, `LengthOverflow`).
    pub fn journal_with(
        &self,
        path: impl AsRef<std::path::Path>,
        options: crate::JournalOptions,
    ) -> Result<crate::JournalHandle> {
        let resolved = self.resolve_path(path.as_ref())?;
        let mut journal = crate::journal::options::open_with_options(&resolved, options)?;
        journal.set_observer(self.observer.clone());
        Ok(journal)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Crate-internal helpers
    // ──────────────────────────────────────────────────────────────────────────

    /// Updates the active method after a runtime fallback.
    ///
    /// Called by IO functions when the OS rejects a privileged flag (e.g.
    /// `O_DIRECT` on tmpfs). Takes effect for all subsequent operations on
    /// this handle.
    pub(crate) fn update_active_method(&self, method: Method) {
        self.active_method.store(method.to_u8(), Ordering::Relaxed);
    }

    /// Returns `true` if the active method requires Direct IO.
    pub(crate) fn use_direct(&self) -> bool {
        self.active_method() == Method::Direct
    }

    /// Resolves a caller-supplied path against this handle's root.
    ///
    /// **Security contract.** The handle's `root` was canonicalised
    /// at [`Builder::build`] time (all symlinks resolved). This
    /// function performs lexical normalisation of the caller's path,
    /// then **re-canonicalises the longest existing prefix** of the
    /// resolved path and verifies it still lies inside the canonical
    /// root. That second check catches the case where a symlink
    /// inside the root points outside it: the lexical `starts_with`
    /// check would pass, but the canonical-prefix check rejects.
    ///
    /// For paths whose target does not yet exist (e.g. `write` to a
    /// new file), only the existing prefix is canonicalised; the
    /// not-yet-existing tail components are joined back lexically.
    /// A component is treated as "not yet existing" only when
    /// `lstat` reports it missing. A symlink that cannot be resolved
    /// (dangling or looping) is rejected with [`Error::InvalidPath`]:
    /// creating opens such as [`Handle::append`] follow the link, so
    /// accepting it would let a link inside the root create files
    /// wherever it points.
    ///
    /// If the handle has no root, the path is returned as-is.
    ///
    /// **Known gap (TOCTOU).** A truly hostile local actor could
    /// race a symlink swap between this resolution and the
    /// subsequent `open`. The 1.0 mitigation is a platform-specific
    /// `openat2(RESOLVE_BENEATH)` (Linux 5.6+) /
    /// `O_NOFOLLOW`-walked openat (POSIX) /
    /// `FILE_FLAG_OPEN_REPARSE_POINT` (Windows) primitive that
    /// closes the race entirely. Filed for 0.9.0+; for 0.8.0 alpha
    /// the lexical + canonical-prefix check is the documented
    /// guarantee.
    pub(crate) fn resolve_path(&self, path: &Path) -> Result<PathBuf> {
        let Some(root) = &self.root else {
            return Ok(path.to_owned());
        };

        let candidate = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };

        // Pass 1 — lexical normalisation. Catches `..` traversal
        // that exceeds the root depth before any syscall fires.
        let mut resolved = PathBuf::new();
        for component in candidate.components() {
            use std::path::Component;
            match component {
                Component::Prefix(p) => {
                    resolved.push(p.as_os_str());
                }
                Component::RootDir => {
                    resolved.push(component);
                }
                Component::CurDir => {
                    // Skip `.`
                }
                Component::ParentDir => {
                    if !resolved.pop() {
                        return Err(Error::InvalidPath {
                            path: path.to_owned(),
                            reason: "path escapes the handle root".into(),
                        });
                    }
                }
                Component::Normal(n) => {
                    resolved.push(n);
                }
            }
        }

        // Pass 2 — lexical `starts_with(root)` check on the
        // normalised path. Cheap; rejects obvious escapes before
        // we touch the filesystem.
        if !rootpath::starts_with(&resolved, root) {
            return Err(Error::InvalidPath {
                path: path.to_owned(),
                reason: "path escapes the handle root (lexical)".into(),
            });
        }

        // 0.8.0 I round-3 fast path. The expensive Pass 3
        // (`canonicalize` syscall on every op) is a 50–200 µs cost
        // on Windows. The vast majority of operations fall into a
        // shape where canonicalize is *unnecessary*:
        //
        //   - The resolved path's parent equals the canonical
        //     root (i.e. the user wrote `fs.write("file.txt")`,
        //     not `fs.write("subdir/file.txt")`).
        //   - The leaf component either doesn't exist yet (write-
        //     new case) or is not a symlink (regular file/dir).
        //
        // For that shape, the security guarantee is preserved by:
        //   1. The canonical-root invariant (Builder::build canonicalised it).
        //   2. The lexical pass-1 normalisation rejecting `..`-escape.
        //   3. A cheap `symlink_metadata` (`lstat`) on the leaf to
        //      reject symlinked-leaf-pointing-outside.
        //
        // Cost of the fast path: 1 `lstat` syscall (~1–5 µs) vs.
        // 1 `canonicalize` syscall (~50–200 µs on Windows). 10×+
        // speedup for the common case.
        //
        // The slow path (Pass 3 below) handles the remaining cases
        // — nested writes (`subdir/file.txt`), reads of paths with
        // symlinks anywhere in the chain, etc.
        if let Some(parent) = resolved.parent() {
            if parent == root.as_path() {
                match std::fs::symlink_metadata(&resolved) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Write-new case — leaf doesn't exist; the parent
                        // is the canonical root; we're safe.
                        return Ok(resolved);
                    }
                    Ok(meta) if !meta.file_type().is_symlink() => {
                        // Existing leaf, not a symlink. Safe.
                        return Ok(resolved);
                    }
                    _ => {
                        // Either the leaf IS a symlink (potentially
                        // pointing outside root) or another error —
                        // fall through to the canonicalize-based
                        // slow path below for proper validation.
                    }
                }
            }
        }

        // Pass 3 — canonicalise the longest existing prefix and
        // verify it still lies inside the canonical root. This is
        // the load-bearing security check: it catches symlinks
        // inside the root that point outside.
        let mut existing_prefix = resolved.clone();
        let mut tail_components: Vec<std::ffi::OsString> = Vec::new();
        loop {
            match std::fs::canonicalize(&existing_prefix) {
                Ok(canon) => {
                    if !canon.starts_with(root) {
                        return Err(Error::InvalidPath {
                            path: path.to_owned(),
                            reason:
                                "path escapes the handle root via symlink (canonical-prefix check)"
                                    .into(),
                        });
                    }
                    // Re-attach any not-yet-existing tail components.
                    let mut out = canon;
                    for tail in tail_components.iter().rev() {
                        out.push(tail);
                    }
                    return Ok(out);
                }
                Err(_) => {
                    // `canonicalize` failed at this depth. Only a
                    // component that genuinely does not exist may be
                    // popped and re-attached lexically. A symlink here
                    // (dangling, looping, or otherwise unresolvable)
                    // must be rejected: popping it would hand the
                    // caller `root/link/...`, and a creating open
                    // (`append`, `write_at`, `sync`, `journal`) would
                    // then follow the link and create its target
                    // wherever it points, including outside the root.
                    match std::fs::symlink_metadata(&existing_prefix) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Ok(meta) if meta.file_type().is_symlink() => {
                            return Err(Error::InvalidPath {
                                path: path.to_owned(),
                                reason: "path contains a symlink that cannot be resolved inside the handle root (dangling or looping link)"
                                    .into(),
                            });
                        }
                        _ => {
                            return Err(Error::InvalidPath {
                                path: path.to_owned(),
                                reason: "path component exists but cannot be canonicalised for the root check"
                                    .into(),
                            });
                        }
                    }
                    // The path doesn't exist at this depth; pop one
                    // component and retry. If we've popped past the
                    // root, the path is unreachable.
                    let popped = match existing_prefix.file_name() {
                        Some(n) => n.to_os_string(),
                        None => {
                            return Err(Error::InvalidPath {
                                path: path.to_owned(),
                                reason: "path has no canonical existing ancestor".into(),
                            });
                        }
                    };
                    tail_components.push(popped);
                    if !existing_prefix.pop() {
                        return Err(Error::InvalidPath {
                            path: path.to_owned(),
                            reason: "path has no canonical existing ancestor".into(),
                        });
                    }
                    // Defensive: if we've popped past the canonical
                    // root, the path can't be inside.
                    if !rootpath::starts_with(&existing_prefix, root) {
                        return Err(Error::InvalidPath {
                            path: path.to_owned(),
                            reason: "no canonical ancestor lies within the handle root".into(),
                        });
                    }
                }
            }
        }
    }

    /// Generates a temp-file path adjacent to `path`.
    ///
    /// The name is `.fsys-tmp-<pid>-<nonce>.<filename>` (pid and nonce
    /// in hex). The pid keeps concurrent processes writing into the
    /// same directory apart; the nonce mixes a per-process counter,
    /// the wall clock and per-thread random keys, so a fresh process
    /// does not regenerate the names of temp files orphaned by an
    /// earlier crash. The prefix keeps temp files identifiable for
    /// crash recovery and sorts them next to each other.
    ///
    /// When the target's file name is so long that the temp name would
    /// exceed the 255-byte component limit, the file name part is
    /// replaced by a 64-bit hash of it.
    ///
    /// Collisions remain theoretically possible, so callers create the
    /// file with an exclusive open and retry with a fresh name on
    /// `AlreadyExists` (see `crud::atomic::with_unique_temp`).
    pub(crate) fn gen_temp_path(path: &Path) -> PathBuf {
        use std::ffi::OsString;
        use std::hash::{BuildHasher, Hash, Hasher};

        let parent = path.parent().unwrap_or_else(|| Path::new("."));

        let counter = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(counter);
        hasher.write_u128(nanos);
        let nonce = hasher.finish();

        let mut temp_name = OsString::with_capacity(64);
        temp_name.push(TEMP_PREFIX);
        temp_name.push(format!("{:x}-{nonce:016x}.", std::process::id()));
        if let Some(name) = path.file_name() {
            // `as_encoded_bytes().len()` is an upper bound on the
            // UTF-16 length on Windows, so the check is conservative
            // on every platform.
            if temp_name.len() + name.as_encoded_bytes().len() <= MAX_NAME_LEN {
                temp_name.push(name);
            } else {
                let mut name_hasher = std::collections::hash_map::DefaultHasher::new();
                name.hash(&mut name_hasher);
                temp_name.push(format!("{:016x}", name_hasher.finish()));
            }
        }
        parent.join(temp_name)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Batch API (0.4.0)
    //
    // Routes through the group-lane pipeline. The pipeline's dispatcher is
    // spawned lazily on first use and shut down cleanly on `Handle` drop.
    // See `pipeline/mod.rs` and `.dev/DECISIONS-0.4.0.md` (D-4, D-5) for the
    // architecture.
    // ──────────────────────────────────────────────────────────────────────────

    /// Atomically writes every `(path, data)` pair in `batch` through the
    /// group lane.
    ///
    /// Ops execute in **strict submission order**. The first failure (a
    /// returned `Err` *or* a panic inside an op) stops the batch — ops
    /// after the failure are **not** attempted. Ops that succeeded before
    /// the failure **are** durable; fsys does not roll them back.
    ///
    /// # Latency characteristics
    ///
    /// Submits to the group lane. **Blocks** if the queue is full (default
    /// capacity 1024 jobs). Returns when every op in this batch has been
    /// processed by the dispatcher and a per-batch result is reported back.
    /// First call to any batch method on this handle spawns the dispatcher
    /// thread (~one-time ~50–200 µs cost).
    ///
    /// With the default single dispatcher
    /// ([`crate::Builder::dispatcher_shards`]), batches submitted to this
    /// handle from any thread execute one after another, so a batch can
    /// wait behind batches submitted earlier by other threads.
    ///
    /// # Errors
    ///
    /// - [`BatchError`] wrapping [`Error::InvalidPath`] if any path
    ///   escapes the handle root. Reported with `failed_at` set to the
    ///   first invalid index and `completed = 0` (path validation
    ///   happens before submission, so nothing was attempted).
    /// - [`BatchError`] wrapping the underlying [`Error`] if a
    ///   per-op IO error occurs in the dispatcher. `failed_at` is the
    ///   op index, `completed` is the count of ops that succeeded
    ///   before it.
    /// - [`BatchError`] wrapping [`Error::ShutdownInProgress`] if the
    ///   handle is being dropped concurrently with this submission
    ///   (effectively unreachable when handle ownership is single-
    ///   threaded or properly fenced).
    pub fn write_batch<P: AsRef<Path>>(
        &self,
        batch: &[(P, &[u8])],
    ) -> std::result::Result<(), BatchError> {
        let mut ops: Vec<BatchOp> = Vec::with_capacity(batch.len());
        for (i, (path, data)) in batch.iter().enumerate() {
            let resolved = self
                .resolve_path(path.as_ref())
                .map_err(|e| pre_submit_err(i, e))?;
            ops.push(BatchOp::Write {
                path: resolved,
                data: data.to_vec(),
            });
        }
        self.submit_batch(ops)
    }

    /// Idempotently deletes every path in `batch` through the group lane.
    ///
    /// Same ordering and failure semantics as [`Handle::write_batch`].
    /// Missing files are not an error (matching solo-lane
    /// [`Handle::delete`]).
    ///
    /// # Latency characteristics
    ///
    /// See [`Handle::write_batch`].
    ///
    /// # Errors
    ///
    /// Same shape as [`Handle::write_batch`]; per-op delete errors are
    /// limited to permission and OS-level failures.
    pub fn delete_batch<P: AsRef<Path>>(&self, batch: &[P]) -> std::result::Result<(), BatchError> {
        let mut ops: Vec<BatchOp> = Vec::with_capacity(batch.len());
        for (i, path) in batch.iter().enumerate() {
            let resolved = self
                .resolve_path(path.as_ref())
                .map_err(|e| pre_submit_err(i, e))?;
            ops.push(BatchOp::Delete { path: resolved });
        }
        self.submit_batch(ops)
    }

    /// Copies every `(src, dst)` pair in `batch` through the group lane.
    ///
    /// Each copy is implemented as `read(src)` followed by an
    /// atomic-replace `write(dst)`, identical to solo-lane
    /// [`Handle::copy`] under the atomic-replace pattern.
    ///
    /// # Latency characteristics
    ///
    /// See [`Handle::write_batch`].
    ///
    /// # Errors
    ///
    /// Same shape as [`Handle::write_batch`]; per-op copy errors include
    /// "source missing" (returns the underlying `Error::Io` with
    /// `ErrorKind::NotFound`).
    pub fn copy_batch<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        batch: &[(P, Q)],
    ) -> std::result::Result<(), BatchError> {
        let mut ops: Vec<BatchOp> = Vec::with_capacity(batch.len());
        for (i, (src, dst)) in batch.iter().enumerate() {
            let resolved_src = self
                .resolve_path(src.as_ref())
                .map_err(|e| pre_submit_err(i, e))?;
            let resolved_dst = self
                .resolve_path(dst.as_ref())
                .map_err(|e| pre_submit_err(i, e))?;
            ops.push(BatchOp::Copy {
                src: resolved_src,
                dst: resolved_dst,
            });
        }
        self.submit_batch(ops)
    }

    /// Returns a [`Batch`] builder bound to this handle.
    ///
    /// The builder accumulates ops via chainable `write` / `delete` /
    /// `copy` calls and submits them all in a single batch when
    /// [`Batch::commit`] is called. Useful for very large or dynamic
    /// batches where building a slice up-front is awkward.
    ///
    /// # Allocation semantics
    ///
    /// Per decision R-15 in `.dev/DECISIONS-0.4.0.md`, the builder
    /// allocates **at each `.write()` / `.delete()` / `.copy()` call**,
    /// not lazily at commit. Allocations are paced; a 10K-op batch pays
    /// 10K small allocations spread across the build loop, not one big
    /// burst at commit.
    pub fn batch(&self) -> Batch<'_> {
        Batch::new(self)
    }

    /// Returns the [`HandleSnapshot`] used by the pipeline dispatcher.
    ///
    /// Captures `active_method`, `sector_size`, and `use_direct` at the
    /// moment of the call. The snapshot travels with each [`BatchJob`]
    /// into the dispatcher; subsequent solo-lane fallbacks on this
    /// handle do not retroactively update jobs already in flight.
    pub(crate) fn snapshot(&self) -> HandleSnapshot {
        HandleSnapshot {
            method: self.active_method(),
            sector_size: self.sector_size,
            use_direct: self.use_direct(),
        }
    }

    /// Submits a pre-resolved op vector through the group-lane pipeline.
    ///
    /// `pub(crate)` — used by [`Batch::commit`] in `batch.rs` to avoid
    /// exposing the pipeline field directly to that module.
    pub(crate) fn submit_batch(&self, ops: Vec<BatchOp>) -> std::result::Result<(), BatchError> {
        self.pipeline.submit(ops, self.snapshot(), false)
    }

    /// 0.9.3: grouped-commit variant of [`Self::submit_batch`].
    /// Routes the same op vector through the dispatcher with the
    /// `grouped` flag set, so per-op `sync_parent_dir` calls are
    /// skipped and replaced by one `sync_parent_dir` per unique
    /// parent directory after the entire batch succeeds. Backs
    /// [`crate::Batch::commit_grouped`].
    pub(crate) fn submit_batch_grouped(
        &self,
        ops: Vec<BatchOp>,
    ) -> std::result::Result<(), BatchError> {
        self.pipeline.submit(ops, self.snapshot(), true)
    }

    /// Async equivalent of [`submit_batch`]. Routes through
    /// [`Pipeline::submit_async`] (locked decision D-5) — same
    /// dispatcher, oneshot response channel.
    #[cfg(feature = "async")]
    pub(crate) async fn submit_batch_async(
        &self,
        ops: Vec<BatchOp>,
    ) -> std::result::Result<(), BatchError> {
        self.pipeline
            .submit_async(ops, self.snapshot(), false)
            .await
    }
}

/// `true` when `FSYS_DISABLE_NATIVE_ASYNC` is set. Read once per
/// process (the variable is a startup switch), so the async substrate
/// check costs no environment lookup per op.
#[cfg(all(target_os = "linux", feature = "async"))]
fn native_async_disabled_by_env() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("FSYS_DISABLE_NATIVE_ASYNC").is_some())
}

/// Volume serial number of the volume holding `file`, used to key the
/// Windows NVMe-passthrough cache. `None` if the query fails.
#[cfg(target_os = "windows")]
fn volume_serial(file: &std::fs::File) -> Option<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    // SAFETY: `BY_HANDLE_FILE_INFORMATION` is a plain C struct of
    // integers and `FILETIME`s (also integers); the all-zero bit
    // pattern is a valid value for it.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` is an open file, so its raw handle is valid for
    // the duration of the call; `info` is a live, writable struct the
    // function fills and does not retain. Failure is reported through
    // the return value.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    (ok != 0).then(|| u64::from(info.dwVolumeSerialNumber))
}

/// Lexical "is inside the root" pre-filter for the root jail.
///
/// `Builder::build` stores the root in canonical form. On Windows that
/// form carries the verbatim prefix (`\\?\C:\...`) and the on-disk case
/// of every component, while callers usually pass `C:\...` in whatever
/// case they like. Plain [`Path::starts_with`] compares prefix kinds and
/// component bytes exactly, so every absolute in-root path was rejected
/// on Windows before reaching the canonical check.
///
/// On Windows this comparison treats `C:` and `\\?\C:` (and
/// `\\server\share` and `\\?\UNC\server\share`) as the same prefix and
/// compares components case-insensitively. It is only a pre-filter:
/// such paths never take `resolve_path`'s fast path (which needs the
/// parent to equal the root exactly) and are decided by the
/// `canonicalize` comparison, which sees on-disk names. A
/// case-sensitive NTFS directory therefore cannot be confused with a
/// sibling whose name differs only in case. On other platforms this is
/// [`Path::starts_with`].
mod rootpath {
    use std::path::Path;

    /// `true` when `path` is `base` or lies below it.
    #[cfg(not(windows))]
    pub(super) fn starts_with(path: &Path, base: &Path) -> bool {
        path.starts_with(base)
    }

    /// `true` when `path` is `base` or lies below it, ignoring the
    /// verbatim prefix and component case.
    #[cfg(windows)]
    pub(super) fn starts_with(path: &Path, base: &Path) -> bool {
        let mut rest = path.components();
        base.components()
            .all(|b| rest.next().is_some_and(|p| win::component_eq(p, b)))
    }

    #[cfg(windows)]
    mod win {
        use std::ffi::OsStr;
        use std::path::{Component, Prefix};

        enum Norm<'a> {
            Drive(u8),
            Unc(&'a OsStr, &'a OsStr),
            Other(&'a OsStr),
        }

        fn norm(p: Prefix<'_>) -> Norm<'_> {
            match p {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Norm::Drive(d.to_ascii_uppercase()),
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    Norm::Unc(server, share)
                }
                Prefix::Verbatim(x) | Prefix::DeviceNS(x) => Norm::Other(x),
            }
        }

        fn name_eq(a: &OsStr, b: &OsStr) -> bool {
            if a == b || a.eq_ignore_ascii_case(b) {
                return true;
            }
            match (a.to_str(), b.to_str()) {
                (Some(a), Some(b)) => a.to_lowercase() == b.to_lowercase(),
                _ => false,
            }
        }

        pub(super) fn component_eq(a: Component<'_>, b: Component<'_>) -> bool {
            match (a, b) {
                (Component::Prefix(pa), Component::Prefix(pb)) => {
                    match (norm(pa.kind()), norm(pb.kind())) {
                        (Norm::Drive(x), Norm::Drive(y)) => x == y,
                        (Norm::Unc(s1, h1), Norm::Unc(s2, h2)) => {
                            name_eq(s1, s2) && name_eq(h1, h2)
                        }
                        (Norm::Other(x), Norm::Other(y)) => x == y,
                        _ => false,
                    }
                }
                (Component::Normal(x), Component::Normal(y)) => name_eq(x, y),
                (x, y) => x == y,
            }
        }
    }
}

/// Builds a [`BatchError`] for a path-validation failure that happens
/// *before* submission. `completed = 0` because no op has been
/// dispatched yet; `failed_at` is the index of the offending op in the
/// caller's slice.
fn pre_submit_err(index: usize, e: Error) -> BatchError {
    BatchError {
        failed_at: index,
        completed: 0,
        source: Box::new(e),
    }
}

// Handle is Send + Sync because AtomicU8 and AtomicU64 are Send + Sync,
// Option<PathBuf> is Send + Sync, Mode is Copy, and u32 is Copy.
// The compiler will derive these automatically, but asserting them here
// makes any future regression a compile error rather than a runtime surprise.
const _: () = {
    #[allow(dead_code)]
    fn assert_send<T: Send>() {}
    #[allow(dead_code)]
    fn assert_sync<T: Sync>() {}
    #[allow(dead_code)]
    fn check() {
        assert_send::<Handle>();
        assert_sync::<Handle>();
    }
};

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use crate::path::Mode;
    use crate::pipeline::PipelineConfig;

    fn default_pool_config() -> HandleBufferPoolConfig {
        HandleBufferPoolConfig {
            capacity: 64,
            block_size: 4096,
            block_align: 512,
        }
    }

    fn make_handle(method: Method) -> Handle {
        Handle::new_raw(
            method,
            method.resolve(),
            None,
            Mode::Dev,
            512,
            Pipeline::new(PipelineConfig::DEFAULT),
            default_pool_config(),
            128,
            None,
            None,
        )
    }

    #[test]
    fn test_method_accessor_roundtrip() {
        let h = make_handle(Method::Sync);
        assert_eq!(h.method(), Method::Sync);
    }

    #[test]
    fn test_active_method_reflects_resolved() {
        let h = make_handle(Method::Auto);
        let active = h.active_method();
        assert_ne!(active, Method::Auto, "active method must be concrete");
    }

    #[test]
    fn test_set_method_updates_active() {
        let h = make_handle(Method::Sync);
        h.set_method(Method::Data).expect("set_method");
        assert_eq!(h.method(), Method::Data);
    }

    #[test]
    fn test_set_reserved_method_returns_error() {
        // 0.5.0: Mmap is no longer reserved — Method::Journal is the
        // only remaining reserved variant (still 0.7.0 work).
        let h = make_handle(Method::Sync);
        let err = h.set_method(Method::Journal);
        assert!(err.is_err());
        if let Err(Error::UnsupportedMethod { method }) = err {
            assert_eq!(method, "journal");
        } else {
            panic!("expected UnsupportedMethod");
        }
    }

    #[test]
    fn test_use_direct_reflects_method() {
        let h = Handle::new_raw(
            Method::Direct,
            Method::Direct,
            None,
            Mode::Dev,
            512,
            Pipeline::new(PipelineConfig::DEFAULT),
            default_pool_config(),
            128,
            None,
            None,
        );
        assert!(h.use_direct());
        let h2 = make_handle(Method::Sync);
        assert!(!h2.use_direct());
    }

    #[test]
    fn test_resolve_path_no_root_passthrough() {
        let h = make_handle(Method::Sync);
        let p = PathBuf::from("some/relative/path");
        assert_eq!(h.resolve_path(&p).expect("resolve"), p);
    }

    #[test]
    fn test_resolve_path_with_root_joins() {
        // 0.8.0: `resolve_path`'s canonical-prefix check requires the
        // stored root to be canonical (which `Builder::build` enforces
        // for all public entry points). `Handle::new_raw` is the
        // pub(crate) backdoor used by tests; pass a canonical root
        // directly.
        let root = std::fs::canonicalize(std::env::temp_dir()).expect("canonicalize temp");
        let h = Handle::new_raw(
            Method::Sync,
            Method::Sync,
            Some(root.clone()),
            Mode::Dev,
            512,
            Pipeline::new(PipelineConfig::DEFAULT),
            default_pool_config(),
            128,
            None,
            None,
        );
        let resolved = h
            .resolve_path(Path::new("subdir/file.txt"))
            .expect("resolve");
        assert!(resolved.starts_with(&root));
    }

    #[test]
    fn test_resolve_path_escape_is_rejected() {
        let root = std::env::temp_dir().join("jail");
        let h = Handle::new_raw(
            Method::Sync,
            Method::Sync,
            Some(root),
            Mode::Dev,
            512,
            Pipeline::new(PipelineConfig::DEFAULT),
            default_pool_config(),
            128,
            None,
            None,
        );
        let result = h.resolve_path(Path::new("../../etc/passwd"));
        assert!(result.is_err(), "path escape must be rejected");
    }

    #[test]
    fn test_gen_temp_path_has_fsys_prefix() {
        let path = PathBuf::from("/tmp/myfile.db");
        let tmp = Handle::gen_temp_path(&path);
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".fsys-tmp-"), "got: {}", name);
        assert!(name.ends_with(".myfile.db"), "got: {}", name);
        assert_eq!(tmp.parent(), path.parent());
    }

    #[test]
    fn test_gen_temp_path_includes_pid_and_differs_per_call() {
        let path = PathBuf::from("/tmp/myfile.db");
        let a = Handle::gen_temp_path(&path);
        let b = Handle::gen_temp_path(&path);
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        let pid = format!(".fsys-tmp-{:x}-", std::process::id());
        assert!(name.starts_with(&pid), "pid missing from {name}");
    }

    #[test]
    fn test_gen_temp_path_does_not_reuse_the_pre_1_1_1_counter_names() {
        // Before 1.1.1 the first temp name of every process was
        // `.fsys-tmp-0.<name>`, so a temp file orphaned by a crash
        // blocked the first write after every restart.
        let path = PathBuf::from("/tmp/myfile.db");
        for _ in 0..64 {
            let name = Handle::gen_temp_path(&path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let rest = name.trim_start_matches(".fsys-tmp-");
            let first = rest.split('.').next().unwrap_or("");
            assert!(first.contains('-'), "counter-only temp name: {name}");
        }
    }

    #[test]
    fn test_gen_temp_path_stays_within_name_limit_for_long_names() {
        for len in [1usize, 200, 215, 216, 240, 255] {
            let long = "n".repeat(len);
            let path = PathBuf::from("/tmp").join(&long);
            let tmp = Handle::gen_temp_path(&path);
            let name = tmp.file_name().unwrap();
            assert!(
                name.len() <= MAX_NAME_LEN,
                "temp name of {} bytes for a {len}-byte target",
                name.len()
            );
            assert!(name.to_string_lossy().starts_with(TEMP_PREFIX));
        }
        // Short names are kept verbatim.
        let tmp = Handle::gen_temp_path(Path::new("/tmp/short.db"));
        assert!(tmp.to_string_lossy().ends_with(".short.db"));
    }

    // ─────────────────────────────────────────────────────────
    // 1.1.1 — root jail vs. dangling symlinks
    // ─────────────────────────────────────────────────────────

    struct DirCleanup(PathBuf);
    impl Drop for DirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn jail_dirs(tag: &str) -> (PathBuf, PathBuf, DirCleanup) {
        let base = std::env::temp_dir().join(format!(
            "fsys_jail_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        (root, outside, DirCleanup(base))
    }

    #[cfg(unix)]
    fn make_link(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    // Creating symlinks on Windows needs the SeCreateSymbolicLink
    // privilege (admin or Developer Mode). Without it the symlink
    // tests return early; the junction test below still covers the
    // dangling-link branch on Windows.
    #[cfg(windows)]
    fn make_link(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }

    #[cfg(windows)]
    fn make_dir_link(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    #[cfg(unix)]
    fn make_dir_link(target: &Path, link: &Path) -> bool {
        make_link(target, link)
    }

    #[test]
    fn test_dangling_leaf_symlink_out_of_root_is_rejected_for_creating_ops() {
        let (root, outside, _g) = jail_dirs("leaf");
        let victim = outside.join("created_through_link");
        if !make_link(&victim, &root.join("evil")) {
            return;
        }
        let h = crate::Builder::new()
            .method(Method::Sync)
            .root(&root)
            .build()
            .expect("build");

        assert!(matches!(
            h.append("evil", b"x"),
            Err(Error::InvalidPath { .. })
        ));
        assert!(matches!(
            h.write_at("evil", 0, b"x"),
            Err(Error::InvalidPath { .. })
        ));
        assert!(matches!(h.sync("evil"), Err(Error::InvalidPath { .. })));
        assert!(matches!(
            h.write("evil", b"x"),
            Err(Error::InvalidPath { .. })
        ));
        assert!(!victim.exists(), "a file was created outside the root");
    }

    #[test]
    fn test_dangling_dir_symlink_out_of_root_is_rejected() {
        let (root, outside, _g) = jail_dirs("dir");
        let missing_dir = outside.join("missing_dir");
        if !make_dir_link(&missing_dir, &root.join("evil_dir")) {
            return;
        }
        let h = crate::Builder::new()
            .method(Method::Sync)
            .root(&root)
            .build()
            .expect("build");
        assert!(matches!(
            h.resolve_path(Path::new("evil_dir/sub/file.bin")),
            Err(Error::InvalidPath { .. })
        ));
        assert!(matches!(
            h.append("evil_dir/file.bin", b"x"),
            Err(Error::InvalidPath { .. })
        ));
        assert!(!missing_dir.exists());
    }

    // Directory junctions need no privilege on Windows, so this test
    // covers the dangling-link branch there even without symlink
    // rights.
    #[cfg(windows)]
    #[test]
    fn test_dangling_junction_out_of_root_is_rejected() {
        let (root, outside, _g) = jail_dirs("junction");
        let missing_dir = outside.join("missing_dir");
        let link = root.join("evil_junction");
        let created = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&missing_dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !created || std::fs::symlink_metadata(&link).is_err() {
            // `mklink /J` unavailable on this host; nothing to test.
            return;
        }
        let h = crate::Builder::new()
            .method(Method::Sync)
            .root(&root)
            .build()
            .expect("build");
        assert!(matches!(
            h.resolve_path(Path::new("evil_junction/file.bin")),
            Err(Error::InvalidPath { .. })
        ));
        assert!(!missing_dir.exists());
    }

    #[cfg(windows)]
    #[test]
    fn test_absolute_in_root_path_without_verbatim_prefix_is_accepted() {
        let (root, outside, _g) = jail_dirs("verbatim");
        let h = crate::Builder::new()
            .method(Method::Sync)
            .root(&root)
            .build()
            .expect("build");
        let canonical_root = h.root().expect("root").to_path_buf();
        assert!(
            canonical_root.to_string_lossy().starts_with(r"\\?\"),
            "canonical root is expected in verbatim form: {}",
            canonical_root.display()
        );

        // `root` is the caller's plain `C:\...` form.
        let plain = root.join("plain.bin");
        h.write(&plain, b"plain").expect("absolute in-root write");
        assert_eq!(std::fs::read(&plain).expect("read"), b"plain");

        // Different case of the same (case-insensitive) directory.
        let upper = PathBuf::from(root.to_string_lossy().to_uppercase()).join("upper.bin");
        h.write(&upper, b"upper")
            .expect("case-insensitive in-root write");
        assert_eq!(
            std::fs::read(root.join("upper.bin")).expect("read"),
            b"upper"
        );

        // Nested, not-yet-existing directories under the root.
        let nested = root.join("a").join("b").join("c.bin");
        let resolved = h.resolve_path(&nested).expect("nested in-root path");
        assert!(resolved.starts_with(&canonical_root));

        // A sibling directory is still outside the root.
        assert!(matches!(
            h.write(outside.join("x.bin"), b"x"),
            Err(Error::InvalidPath { .. })
        ));
        let sibling = PathBuf::from(format!("{}2", root.display())).join("x.bin");
        assert!(matches!(
            h.resolve_path(&sibling),
            Err(Error::InvalidPath { .. })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn test_rootpath_prefix_normalisation() {
        use rootpath::starts_with;
        let verbatim = Path::new(r"\\?\C:\Data\Root");
        assert!(starts_with(Path::new(r"C:\Data\Root\f"), verbatim));
        assert!(starts_with(Path::new(r"c:\Data\Root"), verbatim));
        assert!(starts_with(Path::new(r"C:\data\root\f"), verbatim));
        assert!(!starts_with(Path::new(r"D:\Data\Root\f"), verbatim));
        assert!(!starts_with(Path::new(r"C:\Data\Root2\f"), verbatim));
        assert!(!starts_with(Path::new(r"C:\Data"), verbatim));
        let unc = Path::new(r"\\?\UNC\Server\Share\dir");
        assert!(starts_with(Path::new(r"\\server\share\dir\f"), unc));
        assert!(!starts_with(Path::new(r"\\server\other\dir\f"), unc));
    }

    #[test]
    fn test_missing_components_inside_root_still_resolve() {
        let (root, _outside, _g) = jail_dirs("missing_ok");
        let h = crate::Builder::new()
            .method(Method::Sync)
            .root(&root)
            .build()
            .expect("build");
        let resolved = h
            .resolve_path(Path::new("not/yet/created.bin"))
            .expect("non-existent tail is allowed");
        assert!(resolved.starts_with(h.root().expect("root")));
        assert!(resolved.ends_with("not/yet/created.bin"));
    }

    #[test]
    fn test_device_keyed_probes_once_and_only_serves_the_probed_device() {
        let slot: DeviceKeyed<u32> = DeviceKeyed::new();
        assert!(!slot.is_active());
        let mut probes = 0;
        assert_eq!(
            slot.get(7, || {
                probes += 1;
                Some(42)
            })
            .as_deref(),
            Some(&42)
        );
        assert!(slot.is_active());
        // Same device: cached value, no second probe.
        assert_eq!(slot.get(7, || panic!("re-probed")).as_deref(), Some(&42));
        // Another device: never handed the first device's capability.
        assert_eq!(slot.get(8, || panic!("re-probed")), None);
        assert_eq!(probes, 1);

        let failed: DeviceKeyed<u32> = DeviceKeyed::new();
        assert_eq!(failed.get(1, || None), None);
        assert!(!failed.is_active());
        assert_eq!(failed.get(1, || Some(5)), None, "failure is cached");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_volume_serial_is_stable_for_one_volume() {
        let dir = std::env::temp_dir();
        let a = dir.join(format!("fsys_volserial_a_{}", std::process::id()));
        let b = dir.join(format!("fsys_volserial_b_{}", std::process::id()));
        let fa = std::fs::File::create(&a).expect("create a");
        let fb = std::fs::File::create(&b).expect("create b");
        let sa = volume_serial(&fa);
        let sb = volume_serial(&fb);
        drop((fa, fb));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        assert!(sa.is_some());
        assert_eq!(sa, sb);
    }

    #[test]
    fn test_sector_size_accessor() {
        let h = Handle::new_raw(
            Method::Sync,
            Method::Sync,
            None,
            Mode::Dev,
            4096,
            Pipeline::new(PipelineConfig::DEFAULT),
            default_pool_config(),
            128,
            None,
            None,
        );
        assert_eq!(h.sector_size(), 4096);
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.2 — Hardware-aware accessors
    // ─────────────────────────────────────────────────────────

    #[test]
    fn test_plp_status_is_well_defined() {
        // Sanity: every Handle reports a known PlpStatus variant
        // — the accessor never panics. We don't assert a specific
        // value because it depends on the host hardware.
        let h = make_handle(Method::Sync);
        let status = h.plp_status();
        let _ = matches!(
            status,
            crate::hardware::PlpStatus::Yes
                | crate::hardware::PlpStatus::No
                | crate::hardware::PlpStatus::Unknown
        );
    }

    #[test]
    fn test_is_plp_protected_is_conservative() {
        // The bool form returns `true` ONLY when the underlying
        // status is `Yes`. The CI host is a consumer Windows box
        // where PLP detection always falls into Unknown — pin
        // that mapping.
        let h = make_handle(Method::Sync);
        let bool_form = h.is_plp_protected();
        let status_form = h.plp_status();
        assert_eq!(bool_form, status_form == crate::hardware::PlpStatus::Yes);
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.5 — punch_hole + write_zeros
    // ─────────────────────────────────────────────────────────

    #[test]
    fn test_write_zeros_overwrites_existing_bytes() {
        // Create a file with non-zero data, call write_zeros on
        // a sub-range, confirm the range now reads as zeros.
        let h = make_handle(Method::Sync);
        let path = std::env::temp_dir().join(format!(
            "fsys_handle_write_zeros_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _g = Cleanup(path.clone());

        // Initialise with 16 KiB of 0xAB.
        let payload = vec![0xABu8; 16 * 1024];
        std::fs::write(&path, &payload).expect("seed");
        // Zero bytes [4096..8192].
        h.write_zeros(&path, 4096, 4096).expect("write_zeros");
        // Read back and verify.
        let data = std::fs::read(&path).expect("read back");
        assert_eq!(data.len(), payload.len());
        assert!(
            data[0..4096].iter().all(|&b| b == 0xAB),
            "pre-range untouched"
        );
        assert!(data[4096..8192].iter().all(|&b| b == 0), "range zeroed");
        assert!(
            data[8192..].iter().all(|&b| b == 0xAB),
            "post-range untouched"
        );
    }

    #[test]
    fn test_write_zeros_empty_range_is_noop() {
        // len = 0 must succeed silently without touching the
        // file. Matches the cross-platform contract.
        let h = make_handle(Method::Sync);
        let path = std::env::temp_dir().join(format!(
            "fsys_handle_write_zeros_empty_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _g = Cleanup(path.clone());
        let payload = vec![0xCDu8; 1024];
        std::fs::write(&path, &payload).expect("seed");
        h.write_zeros(&path, 512, 0).expect("write_zeros empty");
        let data = std::fs::read(&path).expect("read back");
        assert_eq!(data, payload, "empty range must not touch the file");
    }

    #[test]
    fn test_punch_hole_zeros_range_on_every_platform() {
        // The cross-platform contract: after punch_hole, the
        // byte range reads as zeros. Block-release behaviour
        // is filesystem-dependent and not directly observable
        // through `std::fs::read`, but the zero-read invariant
        // holds on every supported platform.
        let h = make_handle(Method::Sync);
        let path = std::env::temp_dir().join(format!(
            "fsys_handle_punch_hole_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _g = Cleanup(path.clone());

        // 64 KiB of 0xEE.
        let payload = vec![0xEEu8; 64 * 1024];
        std::fs::write(&path, &payload).expect("seed");
        // Punch a 16 KiB hole at offset 16 KiB.
        match h.punch_hole(&path, 16 * 1024, 16 * 1024) {
            Ok(()) => {
                let data = std::fs::read(&path).expect("read back");
                assert_eq!(data.len(), payload.len(), "file size unchanged");
                assert!(
                    data[0..16 * 1024].iter().all(|&b| b == 0xEE),
                    "pre-hole untouched"
                );
                assert!(
                    data[16 * 1024..32 * 1024].iter().all(|&b| b == 0),
                    "hole reads as zeros"
                );
                assert!(
                    data[32 * 1024..].iter().all(|&b| b == 0xEE),
                    "post-hole untouched"
                );
            }
            Err(crate::Error::Io(e))
                if e.kind() == std::io::ErrorKind::Unsupported
                    || e.raw_os_error() == Some(95) /* EOPNOTSUPP */
                    || e.raw_os_error() == Some(1) /* ERROR_INVALID_FUNCTION */ =>
            {
                // Filesystem doesn't support hole-punching
                // (vfat, certain FUSE mounts, some Windows
                // shares). Test runner may be on such a fs;
                // accept the gap-feature error as documented
                // contract behaviour.
            }
            Err(e) => panic!("unexpected punch_hole error: {e:?}"),
        }
    }

    #[test]
    fn test_observer_field_defaults_to_none() {
        // 0.9.2: a Handle constructed without `Builder::observer`
        // has `observer() == None` and the per-op cost is the
        // single Option::is_some branch.
        let h = make_handle(Method::Sync);
        assert!(h.observer().is_none());
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.4 — NAWUPF accessor
    // ─────────────────────────────────────────────────────────

    #[test]
    fn test_atomic_write_unit_returns_well_defined_option() {
        // The accessor must return either `Some(bytes)` where
        // `bytes` is a positive multiple of the logical sector,
        // or `None`. It must never panic. The actual value
        // depends on host hardware:
        // - Enterprise NVMe with NAWUPF probed: Some(N×sector).
        // - Consumer NVMe / non-Linux: None.
        let h = make_handle(Method::Sync);
        let result = h.atomic_write_unit();
        if let Some(bytes) = result {
            let drive = crate::hardware::drive();
            assert!(
                bytes >= drive.logical_sector,
                "atomic_write_unit ({bytes} bytes) must be at \
                 least one logical sector ({}); NAWUPF is 0-based \
                 so the minimum guarantee is one sector",
                drive.logical_sector
            );
            assert_eq!(
                bytes % drive.logical_sector,
                0,
                "atomic_write_unit must be an integer multiple \
                 of the logical sector size; got {bytes} bytes \
                 with sector {}",
                drive.logical_sector
            );
        }
    }

    #[test]
    fn test_atomic_write_unit_is_conservative() {
        // The conservative-fallback contract: `None` means
        // "fsys cannot confirm an atomic guarantee — protect
        // every write". On a typical Windows / macOS test host
        // the NAWUPF probe lives in the Linux platform layer
        // only, so the field stays `None` and the accessor
        // returns `None`. We can't assert this conditionally
        // across CI hosts without flaky behaviour, but we CAN
        // confirm the round-trip equivalence between the
        // accessor and the underlying drive-info field.
        let h = make_handle(Method::Sync);
        let drive = crate::hardware::drive();
        let accessor_some = h.atomic_write_unit().is_some();
        let field_some = drive.nawupf_lba.is_some();
        assert_eq!(
            accessor_some, field_some,
            "Handle::atomic_write_unit must be Some iff \
             DriveInfo::nawupf_lba is Some"
        );
    }
}
