//! [`AlignedBuffer`] — leased handle into the per-handle buffer pool.
//!
//! The buffer is `Send` (ownership transfers across threads — caller
//! thread fills, dispatcher thread submits to io_uring) but not `Sync`
//! (mutation requires `&mut self` per Rust's normal aliasing rules).
//! `Drop` returns the underlying allocation to the pool's free queue.

use std::alloc::Layout;
use std::ptr::NonNull;
use std::sync::Arc;

use super::pool::PoolInner;

/// Raw aligned allocation tracked inside the pool's free queue.
///
/// Wraps a `NonNull<u8>` + its [`Layout`]. `Send` is hand-implemented
/// because `*mut u8` is `!Send` by default; the buffer is exclusively
/// owned by either the pool's free queue or a single
/// [`AlignedBuffer`] handle, so cross-thread transfer of an
/// exclusively-owned aligned allocation is sound.
pub(super) struct RawAllocation {
    pub(super) ptr: NonNull<u8>,
    pub(super) layout: Layout,
}

// SAFETY: `RawAllocation` represents a heap allocation with a fixed
// layout. At any given time it is owned by exactly one of:
//   (a) the pool's `ArrayQueue<RawAllocation>` free list, or
//   (b) a single `AlignedBuffer` handle.
// Both transfer modes (push to queue, return from queue) are
// well-defined cross-thread transfers of exclusive ownership. There
// is no aliasing and no data race.
unsafe impl Send for RawAllocation {}

/// Aligned buffer leased from an [`super::AlignedBufferPool`].
///
/// Holds:
/// - A [`RawAllocation`] (the actual aligned heap memory + layout).
/// - An `Arc<PoolInner>` so `Drop` can return the allocation to the
///   pool's free queue. The Arc also keeps the pool's `Layout`
///   metadata alive for the buffer's lifetime, even if the
///   [`super::AlignedBufferPool`] itself drops first (which never
///   happens in practice — the pool outlives all leases — but the
///   Arc makes correctness a property of the type system rather
///   than of the call sequence).
pub(crate) struct AlignedBuffer {
    /// `Some(...)` for the lifetime of the lease; `None` only after
    /// `Drop` has moved the allocation back into the pool. Used so
    /// `Drop` can `take` ownership without a partial-move on the
    /// struct.
    raw: Option<RawAllocation>,
    pool: Arc<PoolInner>,
}

impl AlignedBuffer {
    /// Constructs a leased buffer. Internal — only the pool calls this.
    pub(super) fn new(raw: RawAllocation, pool: Arc<PoolInner>) -> Self {
        Self {
            raw: Some(raw),
            pool,
        }
    }

    /// Returns the usable portion of the buffer as a shared slice.
    pub(crate) fn as_slice(&self) -> &[u8] {
        let raw = self.raw_ref();
        // SAFETY: `raw.ptr` was returned by `alloc_zeroed` with
        // `raw.layout` and is valid for `layout.size()` bytes for the
        // lifetime of `self` (we hold an `Arc<PoolInner>` that owns
        // the metadata; the allocation will not be freed until Drop
        // returns it to the pool).
        unsafe { std::slice::from_raw_parts(raw.ptr.as_ptr(), raw.layout.size()) }
    }

    /// Returns the usable portion of the buffer as a mutable slice.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        let raw = self.raw_ref();
        let ptr = raw.ptr.as_ptr();
        let len = raw.layout.size();
        // SAFETY: same reasoning as `as_slice`. `&mut self` proves
        // exclusive access; the slice is valid for `len` bytes.
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }

    /// Returns a raw pointer suitable for `O_DIRECT` / `pwrite` /
    /// `io_uring` SQE submission. Guaranteed aligned to
    /// [`AlignedBuffer::align`].
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.raw_ref().ptr.as_ptr()
    }

    /// Returns a mutable raw pointer. Same alignment guarantee.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.raw_ref().ptr.as_ptr()
    }

    /// Capacity of the buffer in bytes.
    pub(crate) fn len(&self) -> usize {
        self.raw_ref().layout.size()
    }

    /// Alignment of the buffer in bytes (the pool's `block_align`).
    pub(crate) fn align(&self) -> usize {
        self.raw_ref().layout.align()
    }

    fn raw_ref(&self) -> &RawAllocation {
        // `raw` is `Some` for the entire lifetime of `self` until
        // `Drop` runs. `Drop` is the only place that mutates `raw` to
        // `None`, and `Drop` consumes `self`. So inside any non-Drop
        // method, `raw.is_some()` is an invariant.
        match self.raw.as_ref() {
            Some(r) => r,
            None => unreachable_invariant(),
        }
    }
}

#[cold]
#[inline(never)]
fn unreachable_invariant() -> ! {
    // Library code is forbidden from `unwrap`/`expect`/`panic`. This
    // branch is unreachable by the type's `Drop`-only-mutation
    // invariant, but we still need a `!` return to satisfy the type
    // checker. Aborting is the safest "should never happen" sink that
    // does not panic.
    std::process::abort();
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            self.pool.return_allocation(raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::pool::AlignedBufferPool;

    #[test]
    fn test_buffer_len_matches_pool_block_size() {
        let pool = AlignedBufferPool::new(4, 4096, 512).expect("build pool");
        let buf = pool.lease();
        assert_eq!(buf.len(), 4096);
    }

    #[test]
    fn test_buffer_align_matches_pool_block_align() {
        let pool = AlignedBufferPool::new(4, 4096, 512).expect("build pool");
        let buf = pool.lease();
        assert_eq!(buf.align(), 512);
        assert_eq!((buf.as_ptr() as usize) % 512, 0);
    }

    #[test]
    fn test_buffer_as_slice_zero_initialised() {
        let pool = AlignedBufferPool::new(4, 4096, 512).expect("build pool");
        let buf = pool.lease();
        assert!(buf.as_slice().iter().all(|&b| b == 0));
    }

    #[test]
    fn test_buffer_as_mut_slice_writes_visible() {
        let pool = AlignedBufferPool::new(4, 4096, 512).expect("build pool");
        let mut buf = pool.lease();
        buf.as_mut_slice()[0] = 0xAB;
        buf.as_mut_slice()[4095] = 0xCD;
        assert_eq!(buf.as_slice()[0], 0xAB);
        assert_eq!(buf.as_slice()[4095], 0xCD);
    }
}
