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
use std::sync::Mutex;

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

/// Per-handle io_uring ring slot (Linux only).
///
/// Three states:
/// - `Untried`: no Direct op has run yet; the ring has not been
///   probed.
/// - `Active(ring)`: ring construction succeeded; subsequent Direct
///   ops route through it.
/// - `Disabled`: ring construction failed (kernel < 5.1, SECCOMP,
///   container restriction, etc.). Cached so we don't retry on every
///   op; the Direct path falls through to the existing
///   `pwrite`+`fdatasync` fallback.
#[cfg(target_os = "linux")]
enum IoUringState {
    Untried,
    Active(Arc<IoUringRing>),
    Disabled,
}

/// Per-handle NVMe-passthrough capability slot (Linux only).
///
/// Same three-state pattern as [`IoUringState`]. The first Direct
/// op probes via [`crate::platform::linux_iouring::nvme_flush_capable`]
/// and caches the result. `Active(access)` holds an open
/// `/dev/nvmeX` handle plus the namespace ID, ready for
/// `nvme_flush_ioctl` calls. `Disabled` means probing failed; the
/// Direct path uses `fdatasync` instead.
#[cfg(target_os = "linux")]
enum NvmeState {
    Untried,
    Active(Arc<NvmeAccess>),
    Disabled,
}

/// Per-handle native async io_uring substrate slot (Linux + async
/// feature only). Same three-state pattern. Constructed on the
/// first async Direct op. Once `Disabled`, the substrate caches
/// the failure and async ops fall through to `spawn_blocking`.
///
/// New in `0.7.0`.
#[cfg(all(target_os = "linux", feature = "async"))]
enum AsyncIoUringState {
    Untried,
    Active(Arc<AsyncIoUring>),
    Disabled,
}

/// Per-handle NVMe-passthrough capability slot (Windows only).
///
/// Mirror of [`NvmeState`] for the Windows IOCTL path. `Active`
/// holds the resolved volume root (e.g. `\\\\.\\C:`); volume
/// handles are reopened per-op (matches the Windows convention of
/// not holding long-lived shared volume handles).
#[cfg(target_os = "windows")]
enum NvmeStateWin {
    Untried,
    Active(WinArc<WinNvmeAccess>),
    Disabled,
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
// Write-counter for unique temp-file names
// ──────────────────────────────────────────────────────────────────────────────

/// Process-global monotonic counter for generating unique temp-file names.
///
/// Using a global counter (rather than per-handle) ensures uniqueness even
/// when multiple handles share the same root directory.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    pool_slot: Mutex<Option<AlignedBufferPool>>,
    /// Linux-only: requested `io_uring` SQ depth (from
    /// [`crate::Builder::io_uring_queue_depth`]). Captured at
    /// construction; consumed by [`Handle::io_uring_ring`] on the
    /// first Direct-method op.
    #[cfg(target_os = "linux")]
    iouring_queue_depth: u32,
    /// Linux-only: lazy `io_uring` ring slot. `Untried` until the
    /// first Direct op probes; `Active(...)` or `Disabled` for the
    /// rest of this Handle's lifetime.
    #[cfg(target_os = "linux")]
    iouring_slot: Mutex<IoUringState>,
    /// Linux-only: lazy NVMe-passthrough capability slot.
    /// `Untried` until the first Direct op probes; `Active(...)`
    /// (with an owned `/dev/nvmeX` handle) or `Disabled` for the
    /// rest of this Handle's lifetime.
    #[cfg(target_os = "linux")]
    nvme_slot: Mutex<NvmeState>,
    /// Windows-only: lazy NVMe-passthrough capability slot.
    /// Same three-state pattern as [`NvmeState`] but caches the
    /// resolved volume root (handles are reopened per-op).
    #[cfg(target_os = "windows")]
    nvme_slot_win: Mutex<NvmeStateWin>,
    /// Linux + `async` feature only: lazy native io_uring async
    /// substrate slot. New in `0.7.0`.
    #[cfg(all(target_os = "linux", feature = "async"))]
    async_iouring_slot: Mutex<AsyncIoUringState>,
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
    ) -> Self {
        Self {
            configured_method: AtomicU8::new(configured_method.to_u8()),
            active_method: AtomicU8::new(active_method.to_u8()),
            root,
            mode,
            sector_size,
            pipeline,
            pool_config,
            pool_slot: Mutex::new(None),
            #[cfg(target_os = "linux")]
            iouring_queue_depth,
            #[cfg(target_os = "linux")]
            iouring_slot: Mutex::new(IoUringState::Untried),
            #[cfg(target_os = "linux")]
            nvme_slot: Mutex::new(NvmeState::Untried),
            #[cfg(target_os = "windows")]
            nvme_slot_win: Mutex::new(NvmeStateWin::Untried),
            #[cfg(all(target_os = "linux", feature = "async"))]
            async_iouring_slot: Mutex::new(AsyncIoUringState::Untried),
        }
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
        let mut guard = match self.async_iouring_slot.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match &*guard {
            AsyncIoUringState::Active(a) => return Some(a.clone()),
            AsyncIoUringState::Disabled => return None,
            AsyncIoUringState::Untried => {}
        }
        match AsyncIoUring::new(self.iouring_queue_depth) {
            Ok(ring) => {
                let arc = Arc::new(ring);
                *guard = AsyncIoUringState::Active(arc.clone());
                Some(arc)
            }
            Err(_) => {
                *guard = AsyncIoUringState::Disabled;
                None
            }
        }
    }

    /// Returns the per-handle Windows NVMe-passthrough access for
    /// the volume containing `path`, probing on the first call.
    /// Cached `None` after probe failure.
    #[cfg(target_os = "windows")]
    pub(crate) fn nvme_access_win(&self, path: &Path) -> Option<WinArc<WinNvmeAccess>> {
        let mut guard = match self.nvme_slot_win.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match &*guard {
            NvmeStateWin::Active(a) => return Some(a.clone()),
            NvmeStateWin::Disabled => return None,
            NvmeStateWin::Untried => {}
        }
        match crate::platform::windows_nvme::nvme_flush_capable(path) {
            Some(access) => {
                let arc = WinArc::new(access);
                *guard = NvmeStateWin::Active(arc.clone());
                Some(arc)
            }
            None => {
                *guard = NvmeStateWin::Disabled;
                None
            }
        }
    }

    /// Returns the per-handle NVMe passthrough access, probing on
    /// the first call given an arbitrary file `fd` whose underlying
    /// block device we want to flush. The probe resolves the fd to
    /// `/dev/nvmeX` and verifies privilege.
    ///
    /// Cached `None` after probe failure so subsequent ops don't
    /// retry the resolution + open.
    #[cfg(target_os = "linux")]
    pub(crate) fn nvme_access(&self, fd: std::os::fd::RawFd) -> Option<Arc<NvmeAccess>> {
        let mut guard = match self.nvme_slot.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match &*guard {
            NvmeState::Active(a) => return Some(a.clone()),
            NvmeState::Disabled => return None,
            NvmeState::Untried => {}
        }
        match crate::platform::linux_iouring::nvme_flush_capable(fd) {
            Some(access) => {
                let arc = Arc::new(access);
                *guard = NvmeState::Active(arc.clone());
                Some(arc)
            }
            None => {
                *guard = NvmeState::Disabled;
                None
            }
        }
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
        let nvme_active = matches!(
            *self.nvme_slot.lock().unwrap_or_else(|p| p.into_inner()),
            NvmeState::Active(_)
        );
        if nvme_active {
            return crate::primitive::IO_URING_NVME_FLUSH;
        }
        let ring_active = matches!(
            *self.iouring_slot.lock().unwrap_or_else(|p| p.into_inner()),
            IoUringState::Active(_)
        );
        if ring_active {
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
        let nvme_active = matches!(
            *self.nvme_slot_win.lock().unwrap_or_else(|p| p.into_inner()),
            NvmeStateWin::Active(_)
        );
        if nvme_active {
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
        if std::env::var_os("FSYS_DISABLE_NATIVE_ASYNC").is_some() {
            return false;
        }
        if self.active_method() != Method::Direct {
            return false;
        }
        // Only report native when the ASYNC ring is constructed and
        // its driver isn't poisoned. The first async Direct op finds
        // SpawnBlocking (async ring not yet constructed), routes
        // through spawn_blocking; the next op finds the cached
        // result. We also check the poisoned flag — a panicked
        // driver is functionally fallback even if the slot says
        // Active.
        let guard = match self.async_iouring_slot.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        matches!(&*guard, AsyncIoUringState::Active(ring) if !ring.is_poisoned())
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
        let mut guard = match self.iouring_slot.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match &*guard {
            IoUringState::Active(r) => return Some(r.clone()),
            IoUringState::Disabled => return None,
            IoUringState::Untried => {}
        }
        match IoUringRing::new(self.iouring_queue_depth) {
            Ok(ring) => {
                let arc = Arc::new(ring);
                *guard = IoUringState::Active(arc.clone());
                Some(arc)
            }
            Err(_) => {
                *guard = IoUringState::Disabled;
                None
            }
        }
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
        let mut guard = match self.pool_slot.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(pool) = guard.as_ref() {
            return Ok(pool.clone());
        }
        let pool = AlignedBufferPool::new(
            self.pool_config.capacity,
            self.pool_config.block_size,
            self.pool_config.block_align,
        )?;
        *guard = Some(pool.clone());
        Ok(pool)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Public accessors
    // ──────────────────────────────────────────────────────────────────────────

    /// Returns the method that was configured by the caller.
    ///
    /// This may be [`Method::Auto`] if the caller did not specify a method;
    /// see [`Handle::active_method`] for the resolved value.
    #[must_use]
    pub fn method(&self) -> Method {
        Method::from_u8(self.configured_method.load(Ordering::Relaxed))
    }

    /// Returns the method currently in effect after any runtime fallbacks.
    ///
    /// This is always a concrete method (`Sync`, `Data`, or `Direct`) —
    /// never `Auto`. If `O_DIRECT` was rejected at open time and the
    /// handle fell back to `Data`, this method will reflect that change.
    #[must_use]
    pub fn active_method(&self) -> Method {
        Method::from_u8(self.active_method.load(Ordering::Relaxed))
    }

    /// Updates the configured method for future IO operations.
    ///
    /// Returns [`Error::UnsupportedMethod`] for reserved variants
    /// ([`Method::Mmap`] and [`Method::Journal`]).
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

    /// Returns the root directory scope, if one was configured.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Returns the operating mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Returns the probed logical sector size in bytes.
    ///
    /// Used to size aligned Direct IO buffers.
    #[must_use]
    pub fn sector_size(&self) -> u32 {
        self.sector_size
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
    /// If the handle has a root:
    /// - Absolute paths are checked to ensure they are rooted *inside* the
    ///   handle root (rejects path-escape attacks).
    /// - Relative paths are joined to the root.
    ///
    /// If the handle has no root, the path is returned as-is.
    pub(crate) fn resolve_path(&self, path: &Path) -> Result<PathBuf> {
        let Some(root) = &self.root else {
            return Ok(path.to_owned());
        };

        let candidate = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };

        // Canonicalise components without touching the filesystem so that
        // a path like `root/a/../../../etc/passwd` is caught before any
        // syscall. We do a simple lexical normalisation: process each
        // component and reject `..` that would escape the root.
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

        // Final check: the resolved path must start with the root.
        if !resolved.starts_with(root) {
            return Err(Error::InvalidPath {
                path: path.to_owned(),
                reason: "path escapes the handle root".into(),
            });
        }

        Ok(resolved)
    }

    /// Generates a unique temp-file path adjacent to `path`.
    ///
    /// The temp name is `.fsys-tmp-<counter>.<filename>` so it sorts near
    /// the target and is identifiable in crash recovery. If the target has
    /// no file name the counter alone is used.
    pub(crate) fn gen_temp_path(path: &Path) -> PathBuf {
        let n = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let stem = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = format!(".fsys-tmp-{}.{}", n, stem);
        parent.join(name)
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
        self.pipeline.submit(ops, self.snapshot())
    }

    /// Async equivalent of [`submit_batch`]. Routes through
    /// [`Pipeline::submit_async`] (locked decision D-5) — same
    /// dispatcher, oneshot response channel.
    #[cfg(feature = "async")]
    pub(crate) async fn submit_batch_async(
        &self,
        ops: Vec<BatchOp>,
    ) -> std::result::Result<(), BatchError> {
        self.pipeline.submit_async(ops, self.snapshot()).await
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
        let root = std::env::temp_dir();
        let h = Handle::new_raw(
            Method::Sync,
            Method::Sync,
            Some(root.clone()),
            Mode::Dev,
            512,
            Pipeline::new(PipelineConfig::DEFAULT),
            default_pool_config(),
            128,
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
        );
        assert_eq!(h.sector_size(), 4096);
    }
}
