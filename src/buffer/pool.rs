//! [`AlignedBufferPool`] — per-handle pool of reusable aligned
//! allocations for Direct IO.
//!
//! ## Design
//!
//! Lock-free fast path on top of
//! [`crossbeam_queue::ArrayQueue`] for buffer return + reuse. The
//! slow path (genuinely empty pool, all `capacity` buffers leased)
//! takes a [`std::sync::Mutex`] + [`std::sync::Condvar`] so the
//! caller can `wait` for a returned buffer without spinning.
//!
//! Allocations are lazy: `AlignedBufferPool::new` does not allocate
//! buffer memory. The first `capacity` `lease()` calls allocate
//! fresh `AlignedBuffer`s; subsequent calls reuse from the queue.
//! Idle handles cost zero buffer memory beyond the
//! `Arc<PoolInner>`'s metadata.

use std::alloc::{self, Layout};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crossbeam_queue::ArrayQueue;

use super::aligned::{AlignedBuffer, RawAllocation};
use crate::Error;
use crate::Result;

/// Per-handle aligned buffer pool.
///
/// Cheap to construct — `new` only sets up the `Arc<PoolInner>`.
/// Buffer allocations happen lazily on the first `capacity`
/// `lease()` calls. Cloning the pool is `O(1)` Arc clone — the
/// dispatcher and caller threads can hold cheap clones to lease
/// concurrently.
#[derive(Clone)]
pub(crate) struct AlignedBufferPool {
    inner: Arc<PoolInner>,
}

impl AlignedBufferPool {
    /// Constructs a new pool.
    ///
    /// `capacity` — total buffers the pool can have outstanding.
    /// `block_size` — bytes per buffer. Must be a non-zero multiple
    /// of `block_align`.
    /// `block_align` — alignment in bytes. Must be a power of two and
    /// non-zero.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AlignmentRequired`] when `block_align` is not
    /// a power of two, when `block_size == 0`, or when `block_size`
    /// is not a multiple of `block_align`. These violations make
    /// `Layout::from_size_align` fail and would render every
    /// allocation invalid.
    pub(crate) fn new(capacity: usize, block_size: usize, block_align: usize) -> Result<Self> {
        if block_align == 0 || !block_align.is_power_of_two() {
            return Err(Error::AlignmentRequired {
                detail: "buffer pool block_align must be a non-zero power of two",
            });
        }
        if block_size == 0 || block_size % block_align != 0 {
            return Err(Error::AlignmentRequired {
                detail: "buffer pool block_size must be a non-zero multiple of block_align",
            });
        }
        // Validate the layout produces successfully — catches
        // overflow / zero-size combinations that the above checks
        // theoretically miss.
        let block_layout = Layout::from_size_align(block_size, block_align).map_err(|_| {
            Error::AlignmentRequired {
                detail: "buffer pool layout rejected by Layout::from_size_align",
            }
        })?;

        // ArrayQueue panics on capacity 0; treat that as an error.
        if capacity == 0 {
            return Err(Error::AlignmentRequired {
                detail: "buffer pool capacity must be > 0",
            });
        }

        Ok(Self {
            inner: Arc::new(PoolInner {
                free: ArrayQueue::new(capacity),
                allocated: AtomicUsize::new(0),
                capacity,
                block_layout,
                wait_lock: Mutex::new(()),
                wait_cv: Condvar::new(),
            }),
        })
    }

    /// Leases a buffer from the pool. Blocks if the pool is
    /// exhausted (per checkpoint B' R-2' lock-in) until a buffer is
    /// returned.
    pub(crate) fn lease(&self) -> AlignedBuffer {
        self.inner.lease()
    }

    /// Returns the configured block size in bytes.
    #[allow(dead_code)] // surfaced via Handle integration in F+G
    pub(crate) fn block_size(&self) -> usize {
        self.inner.block_layout.size()
    }

    /// Returns the configured block alignment in bytes.
    #[allow(dead_code)] // surfaced via Handle integration in F+G
    pub(crate) fn block_align(&self) -> usize {
        self.inner.block_layout.align()
    }

    /// Returns the configured pool capacity (max outstanding buffers).
    #[allow(dead_code)] // surfaced via Handle integration in F+G
    pub(crate) fn capacity(&self) -> usize {
        self.inner.capacity
    }
}

/// Internal pool state, held inside an `Arc` and shared between the
/// pool handle and every outstanding [`AlignedBuffer`] (so leased
/// buffers can return to the queue on Drop even if the
/// [`AlignedBufferPool`] handle has been moved/cloned).
pub(super) struct PoolInner {
    free: ArrayQueue<RawAllocation>,
    allocated: AtomicUsize,
    capacity: usize,
    block_layout: Layout,
    /// Companion to `wait_cv`. Held only on the slow path (genuinely
    /// empty pool); the lock-free fast path bypasses it entirely.
    wait_lock: Mutex<()>,
    wait_cv: Condvar,
}

impl PoolInner {
    fn lease(self: &Arc<Self>) -> AlignedBuffer {
        loop {
            // Fast path — try the free queue.
            if let Some(raw) = self.free.pop() {
                return AlignedBuffer::new(raw, Arc::clone(self));
            }

            // Slow path: try to allocate fresh if under capacity.
            let allocated = self.allocated.load(Ordering::Acquire);
            if allocated < self.capacity {
                if self
                    .allocated
                    .compare_exchange(
                        allocated,
                        allocated + 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let raw = match self.alloc_fresh() {
                        Some(r) => r,
                        None => {
                            // Allocator failed. Roll back our reservation
                            // so future leases can retry.
                            let _ = self.allocated.fetch_sub(1, Ordering::AcqRel);
                            // Wait for someone else to return a buffer.
                            self.wait_for_return();
                            continue;
                        }
                    };
                    return AlignedBuffer::new(raw, Arc::clone(self));
                }
                // CAS race lost — another thread took our slot. Retry
                // the loop to see if a buffer became available.
                continue;
            }

            // Pool fully allocated and queue empty: wait for return.
            self.wait_for_return();
            // After being woken, loop re-checks both fast and slow
            // paths.
        }
    }

    fn wait_for_return(&self) {
        // Take the lock and re-check before waiting to avoid lost
        // wakeups: a buffer may have been returned between our last
        // check and the lock acquisition.
        let guard = match self.wait_lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !self.free.is_empty() || self.allocated.load(Ordering::Acquire) < self.capacity {
            return;
        }
        // Bind the lock guard so the `let_underscore_lock` lint is
        // satisfied; the guard drops at the end of this scope which
        // releases the mutex (the wait happens *atomically* on the
        // Condvar regardless).
        let _woken_guard = self.wait_cv.wait(guard);
    }

    fn alloc_fresh(&self) -> Option<RawAllocation> {
        // SAFETY: `block_layout` was validated at pool construction
        // (non-zero size, power-of-two alignment); the allocator
        // contract for `alloc_zeroed` accepts any non-zero-size
        // Layout and returns either a valid pointer or null.
        let ptr = unsafe { alloc::alloc_zeroed(self.block_layout) };
        let nn = NonNull::new(ptr)?;
        Some(RawAllocation {
            ptr: nn,
            layout: self.block_layout,
        })
    }

    /// Returns a leased allocation to the free queue. Called from
    /// [`AlignedBuffer::drop`].
    pub(super) fn return_allocation(&self, raw: RawAllocation) {
        // ArrayQueue::push returns `Err` only if the queue is at
        // capacity. By construction, the queue's capacity equals
        // `self.capacity` and we never have more than that many
        // buffers outstanding, so push always succeeds. Defensive:
        // if it ever fails, deallocate immediately rather than leak.
        if let Err(raw) = self.free.push(raw) {
            // SAFETY: `raw.ptr` was returned by `alloc_zeroed` with
            // `raw.layout` and is currently owned by us (Drop just
            // moved it here). Releasing it now is sound; we also
            // decrement `allocated` so the pool can re-allocate
            // later.
            unsafe {
                alloc::dealloc(raw.ptr.as_ptr(), raw.layout);
            }
            let _ = self.allocated.fetch_sub(1, Ordering::AcqRel);
        }
        // Wake any thread blocked in `wait_for_return`. notify_one
        // is fine because at most one waiter can take this single
        // returned buffer.
        self.wait_cv.notify_one();
    }
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        // Deallocate any buffers still sitting in the free queue. In
        // steady state, the pool drops only after the Handle drops,
        // and at that point all outstanding leases have returned to
        // the queue (or are still alive holding their `Arc<PoolInner>`,
        // which keeps `PoolInner` alive — so `Drop` only runs once
        // truly the last reference is gone).
        while let Some(raw) = self.free.pop() {
            // SAFETY: each `raw` was returned by `alloc_zeroed` with
            // `raw.layout`; we own it exclusively (it just came out
            // of the queue, which had the only reference); freeing
            // is sound.
            unsafe {
                alloc::dealloc(raw.ptr.as_ptr(), raw.layout);
            }
        }
    }
}

// Compile-time assertion: AlignedBufferPool must be Send + Sync so
// it can live inside a Handle that is shared across threads.
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn check() {
        assert_send::<AlignedBufferPool>();
        assert_sync::<AlignedBufferPool>();
        assert_send::<AlignedBuffer>();
    }
    let _ = check;
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_pool_new_validates_alignment_power_of_two() {
        let r = AlignedBufferPool::new(4, 4096, 7);
        assert!(r.is_err(), "non-power-of-two alignment must error");
    }

    #[test]
    fn test_pool_new_validates_size_multiple_of_align() {
        let r = AlignedBufferPool::new(4, 4097, 512);
        assert!(r.is_err(), "size not a multiple of align must error");
    }

    #[test]
    fn test_pool_new_validates_zero_capacity() {
        let r = AlignedBufferPool::new(0, 4096, 512);
        assert!(r.is_err(), "zero capacity must error");
    }

    #[test]
    fn test_pool_new_validates_zero_block_size() {
        let r = AlignedBufferPool::new(4, 0, 512);
        assert!(r.is_err(), "zero block size must error");
    }

    #[test]
    fn test_pool_new_succeeds_with_valid_inputs() {
        let p = AlignedBufferPool::new(64, 4096, 512).expect("valid pool");
        assert_eq!(p.capacity(), 64);
        assert_eq!(p.block_size(), 4096);
        assert_eq!(p.block_align(), 512);
    }

    #[test]
    fn test_lease_returns_aligned_buffer_with_correct_size() {
        let p = AlignedBufferPool::new(4, 4096, 512).expect("pool");
        let b = p.lease();
        assert_eq!(b.len(), 4096);
        assert_eq!(b.align(), 512);
        assert_eq!((b.as_ptr() as usize) % 512, 0);
    }

    #[test]
    fn test_lease_returns_zero_initialised_buffer() {
        let p = AlignedBufferPool::new(4, 4096, 512).expect("pool");
        let b = p.lease();
        assert!(b.as_slice().iter().all(|&x| x == 0));
    }

    #[test]
    fn test_drop_returns_buffer_to_pool() {
        let p = AlignedBufferPool::new(2, 4096, 512).expect("pool");
        // Lease two buffers (allocated count = 2).
        let b1 = p.lease();
        let b2 = p.lease();
        // Capture pointers before drop.
        let p1 = b1.as_ptr() as usize;
        let p2 = b2.as_ptr() as usize;
        drop(b1);
        drop(b2);
        // Subsequent leases must reuse those allocations.
        let b3 = p.lease();
        let b4 = p.lease();
        let p3 = b3.as_ptr() as usize;
        let p4 = b4.as_ptr() as usize;
        // Either order acceptable — pool is a queue, returns FIFO.
        let returned = [p3, p4];
        assert!(returned.contains(&p1), "buf 1 should be reused");
        assert!(returned.contains(&p2), "buf 2 should be reused");
    }

    #[test]
    fn test_concurrent_lease_and_return() {
        let pool = AlignedBufferPool::new(4, 4096, 512).expect("pool");
        let pool_arc = Arc::new(pool);
        let n_threads = 8;
        let leases_per_thread = 16;
        let mut handles = Vec::new();
        for _ in 0..n_threads {
            let p = Arc::clone(&pool_arc);
            handles.push(std::thread::spawn(move || {
                for _ in 0..leases_per_thread {
                    let mut b = p.lease();
                    // Touch the buffer to exercise the pointer.
                    b.as_mut_slice()[0] = 0x42;
                    // Buffer drops at end of iteration → returns to pool.
                }
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }
        // After all threads complete, the pool should still be functional.
        let _ = pool_arc.lease();
    }

    #[test]
    fn test_lazy_allocation_no_buffers_until_first_lease() {
        let p = AlignedBufferPool::new(64, 4096, 512).expect("pool");
        // Before any lease, allocated count must be 0.
        assert_eq!(p.inner.allocated.load(Ordering::Acquire), 0);
        let _b = p.lease();
        // After one lease, allocated == 1.
        assert_eq!(p.inner.allocated.load(Ordering::Acquire), 1);
    }

    #[test]
    fn test_lease_blocks_when_capacity_exhausted_then_succeeds_after_return() {
        // capacity = 1: once leased, the next lease must wait until
        // the first one is returned.
        let pool = AlignedBufferPool::new(1, 4096, 512).expect("pool");
        let pool_arc = Arc::new(pool);

        let b1 = pool_arc.lease();
        let p1_ptr = b1.as_ptr() as usize;

        let p_clone = Arc::clone(&pool_arc);
        let handle = std::thread::spawn(move || {
            // This must block because the only buffer is held.
            let b2 = p_clone.lease();
            b2.as_ptr() as usize
        });

        // Give the spawned thread a moment to reach `wait_for_return`.
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Drop the held buffer — wakes the waiter.
        drop(b1);

        let p2_ptr = handle.join().expect("thread");
        // Same allocation reused.
        assert_eq!(p1_ptr, p2_ptr);
    }
}
