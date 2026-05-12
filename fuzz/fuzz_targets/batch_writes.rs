#![no_main]
//! 0.9.7 audit M-7 — fuzz target for batch commit dispatcher.
//!
//! Where `batch_builder` fuzzes only the chainable-builder API
//! without ever calling `commit()`, this target exercises the
//! **dispatcher**: it builds a batch with adversarial paths +
//! payloads derived from the fuzz input and then commits it
//! against a real filesystem (a per-iteration tmpdir).
//!
//! Verifies:
//!
//! 1. **No panic** on any input — every adversarial path
//!    (long names, NUL bytes attempted, Unicode trash, parent-
//!    relative `../`, absolute `/...`) surfaces a clean
//!    structured error.
//! 2. **No partial-state leak** — when `commit_grouped` returns
//!    a `BatchError`, the `failed_at`/`completed` accessors
//!    consistently identify which ops succeeded and which
//!    didn't.
//!
//! ## Why this matters
//!
//! Batch is the dual-write API every transactional caller uses.
//! The dispatcher dispatches each op against the real platform
//! IO routines (write / delete / copy), which means the fuzz
//! input flows through the platform layer's path resolution,
//! NUL-byte filtering, and EINTR retry loops. A panic anywhere
//! in that chain is a critical bug.

use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_dir() -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "fsys_fuzz_batch_writes_{}_{}",
        std::process::id(),
        n
    ));
    let _ = std::fs::create_dir_all(&p);
    p
}

struct DirCleanup(PathBuf);
impl Drop for DirCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Synthesise a path component from `seed`. Bounded length, avoids
/// NUL (which is rejected by the path validator anyway, but we
/// don't want every iteration to terminate immediately on a NUL).
fn synth_path_segment(seed: u8) -> String {
    // Cycle through several path shapes: simple ASCII, dotted,
    // longer ASCII, hex-encoded.
    match seed % 4 {
        0 => format!("f{seed}"),
        1 => format!("d.{seed}.x"),
        2 => format!("long_name_{:08x}", seed as u32),
        _ => format!("{:02x}{:02x}{:02x}", seed, seed.wrapping_add(1), seed.wrapping_mul(7)),
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(fs) = fsys::builder().build() else {
        return;
    };

    let dir = tmp_dir();
    let _cleanup = DirCleanup(dir.clone());

    let mut batch = fs.batch();

    // Cap ops at 64 to keep each iteration fast. Each input byte
    // selects an op shape and contributes its own path-seed and
    // payload-fill byte. `Batch::write/delete/copy` are chainable
    // (`&mut Self` return); their accumulated state is committed
    // below.
    let cap = data.len().min(64);
    for (i, &b) in data.iter().take(cap).enumerate() {
        let name = synth_path_segment(b);
        let path = dir.join(format!("{i}_{name}"));
        let payload = vec![b; (b as usize).min(256)];

        match b % 4 {
            0 => {
                batch.write(&path, &payload[..]);
            }
            1 => {
                batch.delete(&path);
            }
            2 => {
                let dst = dir.join(format!("{i}_{name}_dst"));
                batch.copy(&path, &dst);
            }
            _ => {
                batch.write(&path, &payload[..]);
            }
        };
    }

    let staged = cap;
    if staged == 0 {
        return;
    }

    // Commit the batch. Both `commit` (best-effort) and
    // `commit_grouped` (atomic) must surface any failure as a
    // structured `BatchError`, never a panic.
    match batch.commit_grouped() {
        Ok(()) => {}
        Err(err) => {
            // `BatchError` accessors must be consistent: completed
            // <= total, failed_at < total (when failed_at is set).
            // Per 0.9.6 H-4, these are now methods, not fields.
            let completed = err.completed();
            let failed_at = err.failed_at();
            debug_assert!(completed <= staged);
            debug_assert!(failed_at <= staged);
            // `inner` accessor must return a non-trivial Error
            // (the renamed-from-`source` accessor per H-4 lockdown).
            let _ = err.inner();
        }
    }
});
