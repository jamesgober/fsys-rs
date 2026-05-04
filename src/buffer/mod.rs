// `AlignedBufferPool` is wired into `Handle` and the Direct method's
// io_uring submission path in checkpoint F+G. The `dead_code` and
// `unused_imports` allowances below cover the gap between checkpoints
// C and F+G for items exercised only by tests until the integration
// lands.
#![allow(dead_code)]
#![allow(unused_imports)]

//! Per-handle aligned buffer pool for Direct IO.
//!
//! Direct IO requires that buffer pointer, file offset, and length all
//! align to the device's logical sector size. The 0.3.0 implementation
//! allocated a fresh aligned buffer per call (`platform::AlignedBuf`);
//! 0.5.0 introduces a per-handle pool of reusable aligned allocations
//! to amortise the allocator cost across the hot Direct-IO path.
//!
//! ## Public surface
//!
//! - [`AlignedBufferPool`] — owned by the Handle, lazily populated on
//!   first Direct-method use.
//! - [`AlignedBuffer`] — leased handle returned by
//!   [`AlignedBufferPool::lease`]. `Drop` returns the buffer to the
//!   pool's free list.
//!
//! Both types are `pub(crate)`; the pool is an internal optimisation
//! and not part of the public crate API.
//!
//! ## Semantics (locked at checkpoint B' in `.dev/DECISIONS-0.5.0.md`)
//!
//! 1. `lease()` blocks on exhaustion until a buffer is returned. A
//!    future opt-in [`crate::Error::BufferPoolExhausted`] error-mode
//!    is reserved (FS-00013).
//! 2. `Drop` on `AlignedBuffer` returns the underlying allocation to
//!    the pool's lock-free free queue.
//! 3. Lazy allocation: `AlignedBufferPool::new` only sets up the
//!    `Arc<PoolInner>`. The first `count` `lease()` calls allocate
//!    fresh buffers; subsequent calls reuse from the queue.
//! 4. Cross-thread leases are safe — `Arc<PoolInner>` is `Send + Sync`.
//! 5. `AlignedBuffer` is `Send` (ownership transfers across threads)
//!    but not `Sync` (mutation is `&mut self`).
//! 6. In-flight `AlignedBuffer`s outlive the pool — they hold an
//!    `Arc<PoolInner>` keeping the allocator metadata alive.

mod aligned;
mod pool;

pub(crate) use aligned::AlignedBuffer;
pub(crate) use pool::AlignedBufferPool;
