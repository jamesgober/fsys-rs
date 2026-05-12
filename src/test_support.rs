//! 0.9.7 H-7 — test-only allocator hooks for OOM injection.
//!
//! Replaces the system global allocator with a forwarding wrapper
//! that consults a thread-local size threshold; allocations at or
//! above the threshold return `null` (OOM) so tests can validate
//! that fsys's fallible-alloc paths (`AlignedBuf::new`,
//! `read_all_direct`, etc.) gracefully surface
//! `Error::Io(OutOfMemory)` rather than panicking.
//!
//! ## Production safety
//!
//! This module compiles only when the **internal** `oom_inject`
//! cargo feature is enabled. The feature is documented as
//! "NEVER enable in production builds" — every allocation pays
//! a thread-local lookup + comparison. Default builds do not
//! pull this code in at all; the global allocator stays the
//! system default.
//!
//! ## Test isolation
//!
//! The threshold is a `thread_local!` value. Tests on different
//! threads have independent budgets; tests on the same thread
//! run sequentially under the default test runner. The
//! `OomThreshold` drop guard restores the threshold to
//! `usize::MAX` (no failures) on scope exit, including unwinding
//! on panic — so a panicking test never poisons subsequent tests.
//! (Plain code-formatting rather than an intra-doc link because
//! this module is `#[doc(hidden)]` from the published surface and
//! rustdoc rejects same-module intra-doc links to `pub(crate)`
//! / hidden items under `-D warnings`.)
//!
//! ## Targeted injection
//!
//! Rather than the count-based "fail the Nth alloc" approach
//! (fragile — sensitive to incidental allocations in the test
//! framework, the path being tested, etc.), this module uses
//! the **size-based** approach: tests set the threshold to
//! `S` and any allocation of `>= S` bytes fails. fsys's
//! load-bearing allocations are typically large (sector-
//! aligned buffers, log-buffer slots — 4 KiB to 64 KiB);
//! ordinary `Vec`/`String`/`PathBuf` allocations are smaller
//! and pass through unaffected.

#![cfg(feature = "oom_inject")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// Minimum allocation size, in bytes, that triggers
    /// OOM injection. `usize::MAX` (default) means "never inject".
    /// Tests set this lower to force failures at known
    /// allocation sites.
    pub static OOM_THRESHOLD: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Forwarding allocator that respects [`OOM_THRESHOLD`].
///
/// Allocations of `< threshold` bytes go to the system
/// allocator unchanged. Allocations of `>= threshold` bytes
/// return `null` (OOM), letting `AlignedBuf::new` /
/// `Vec::try_reserve_exact` / equivalent fallible-alloc paths
/// surface clean errors.
pub struct OomInjectingAllocator;

// SAFETY: `OomInjectingAllocator` forwards every successful
// allocation/deallocation to `System` (the default `GlobalAlloc`
// implementation) unchanged. The only deviation from `System` is
// returning a null pointer when the requested size meets the
// thread-local threshold — which is a permitted `GlobalAlloc::alloc`
// outcome (signalling OOM). `dealloc` always forwards to `System`,
// matching the (`ptr`, `layout`) pair the caller received from us
// (i.e., from `System`). No additional invariants are introduced.
unsafe impl GlobalAlloc for OomInjectingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let threshold = OOM_THRESHOLD.with(|c| c.get());
        if layout.size() >= threshold {
            return std::ptr::null_mut();
        }
        // SAFETY: caller's `layout` is valid (Rust alloc contract);
        // we forward it unmodified to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was either returned by our `alloc` call
        // (in which case it's a System allocation with `layout`)
        // or is null (we returned null on OOM, which the caller
        // should not have passed to dealloc — but System.dealloc
        // would also reject it safely).
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let threshold = OOM_THRESHOLD.with(|c| c.get());
        if layout.size() >= threshold {
            return std::ptr::null_mut();
        }
        // SAFETY: same as `alloc` — forward to System.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let threshold = OOM_THRESHOLD.with(|c| c.get());
        if new_size >= threshold {
            return std::ptr::null_mut();
        }
        // SAFETY: caller invariants on (`ptr`, `layout`, `new_size`)
        // are the GlobalAlloc::realloc contract; we forward.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// RAII guard that restores [`OOM_THRESHOLD`] to its prior value
/// on drop — including panic unwinding.
///
/// Tests construct this with the desired threshold, run the code
/// under test, and the guard restores the threshold on scope
/// exit. A panic in the test body still restores cleanly because
/// Drop runs during unwinding.
///
/// ```ignore
/// #[test]
/// fn aligned_buf_handles_oom() {
///     let _guard = OomThreshold::set(2048);
///     let result = AlignedBuf::new(4096, 4096);
///     assert!(result.is_err());
///     // `_guard` drops here, threshold restored to prior value.
/// }
/// ```
pub struct OomThreshold {
    prior: usize,
}

impl OomThreshold {
    /// Set the thread-local threshold to `bytes`. Returns a
    /// guard that restores the prior value on drop.
    #[must_use]
    pub fn set(bytes: usize) -> Self {
        let prior = OOM_THRESHOLD.with(|c| {
            let p = c.get();
            c.set(bytes);
            p
        });
        Self { prior }
    }
}

impl Drop for OomThreshold {
    fn drop(&mut self) {
        OOM_THRESHOLD.with(|c| c.set(self.prior));
    }
}
