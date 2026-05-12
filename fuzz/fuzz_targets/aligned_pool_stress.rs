#![no_main]
//! 0.9.7 audit M-7 — stress fuzz of the aligned-buffer-pool path.
//!
//! Exercises the `Method::Direct` write path with adversarial
//! payload sizes (zero, exact sector boundaries, one-byte-past,
//! many-multiple-sectors). The Direct path is the only consumer
//! of `AlignedBufferPool`; fuzzing the public API drives the pool
//! through every lease / return / re-lease cycle with fuzz-derived
//! payload shapes.
//!
//! Verifies:
//!
//! 1. **No panic** on any size — the pool's lease / return /
//!    drop paths must be panic-free for every payload shape.
//! 2. **No leak** — every lease must return to the pool when its
//!    `AlignedBuffer` drops, or the next lease will allocate
//!    fresh memory. We can't observe leak directly from
//!    user-space, but the fuzzer's bounded iteration count + the
//!    pool's bounded queue cap means a leak would surface as
//!    ever-growing memory (libFuzzer reports OOM).
//! 3. **Round-trip fidelity** — the bytes written via Direct must
//!    read back identically (sector padding is internal to the
//!    pool and must not leak to userspace).
//!
//! ## Why this matters
//!
//! `AlignedBufferPool` is the load-bearing allocator for every
//! Direct-IO operation. A single panic or leak in the pool's
//! free-queue management surfaces as a Direct write failure in
//! production. Fuzzing it via the public Direct API also
//! exercises the platform-layer's sector-rounding logic, which
//! is the most error-prone code in the Direct path.

use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path() -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_fuzz_aligned_pool_{}_{}",
        std::process::id(),
        n
    ))
}

/// Synthesise a payload size from the first byte of the fuzz
/// input. The chosen sizes target boundary cases that historically
/// surfaced bugs:
///
/// - 0 bytes (rejected by aligned-buf allocator → must error
///   cleanly).
/// - 1 byte (smallest non-zero; must round up to sector size
///   internally without leaking padding to userspace).
/// - 4095 / 4096 / 4097 bytes (one-sector boundary on most
///   systems).
/// - 8192 bytes (exact two sectors).
/// - 12288 bytes (exact three sectors).
/// - 65536 bytes (16 sectors — exercises larger allocations).
fn synth_size(seed: u8) -> usize {
    const SIZES: [usize; 8] = [0, 1, 4095, 4096, 4097, 8192, 12288, 65536];
    SIZES[(seed as usize) % SIZES.len()]
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let Ok(fs) = fsys::builder().method(fsys::Method::Direct).build() else {
        return;
    };

    // Cap the number of Direct writes per iteration. Each write
    // leases an aligned buffer from the pool, writes it, and
    // returns the lease on drop. Cycling many times stresses the
    // pool's queue management.
    let cap = data.len().min(16);
    for (i, &b) in data.iter().take(cap).enumerate() {
        let path = tmp_path();
        let size = synth_size(b);
        // Synthesise payload by repeating the fill byte.
        let fill = data.get(i.wrapping_add(1)).copied().unwrap_or(b);
        let payload: Vec<u8> = vec![fill; size];

        // Direct write. Zero-byte payloads may error cleanly;
        // any non-zero payload must succeed (modulo Direct
        // downgrade on incompatible filesystems).
        let write_result = fs.write(&path, &payload);
        if write_result.is_ok() && size > 0 {
            // Read-back must match exactly (no padding leak).
            if let Ok(read) = fs.read(&path) {
                debug_assert_eq!(
                    read,
                    payload,
                    "Direct-mode read-back diverged for size {size}",
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }
});
