//! 0.4.0 integration: clean shutdown semantics on `Handle` drop.
//!
//! Validates the shutdown protocol from `pipeline/mod.rs`'s `Drop`
//! impl: send shutdown, drop job_tx, wait on done_rx with 5 s timeout.
//! Idle handles cost zero threads (lazy spawn), and active handles
//! drain in-flight batches before exiting.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_pipe_shutdown_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

#[test]
fn drop_idle_handle_is_immediate() {
    // No batch ever submitted → dispatcher never spawned. Drop must be
    // near-instant.
    let start = Instant::now();
    {
        let _h = fsys::new().expect("handle");
        // Use the handle for solo IO only — no batches.
        // (Touching it via solo path doesn't spawn the dispatcher.)
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "idle handle drop should be fast, got {elapsed:?}"
    );
}

#[test]
fn drop_active_handle_completes_within_timeout() {
    // Handle with active dispatcher (one batch submitted). Drop must
    // signal shutdown, drain remaining work, exit within the 5 s
    // hard timeout from `Pipeline::drop`.
    let start = Instant::now();
    {
        let h = fsys::new().expect("handle");
        let p = tmp("active_drop");
        let _ = h.write_batch(&[(p.as_path(), b"x".as_slice())]);
        let _ = std::fs::remove_file(&p);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "active handle drop should not stall, got {elapsed:?}"
    );
}

#[test]
fn drop_after_many_batches_drains_cleanly() {
    let start = Instant::now();
    let mut paths = Vec::new();
    {
        let h = fsys::new().expect("handle");
        for i in 0..20 {
            let p = tmp(&format!("drain_{i}"));
            paths.push(p.clone());
            let _ = h.write_batch(&[(p.as_path(), format!("v{i}").as_bytes())]);
        }
    } // drop here
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "drop after many batches must complete within timeout, got {elapsed:?}"
    );
    // All batches were submitted before drop and the submit call is
    // synchronous (waits for completion), so all files must be on disk.
    for p in &paths {
        assert!(p.exists(), "{p:?} should exist post-drop");
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn many_handles_drop_independently() {
    // Decision #6: per-handle dispatcher. Spawning many handles, each
    // with its own dispatcher, must drop independently and quickly.
    let n = 8;
    let start = Instant::now();
    {
        let mut handles = Vec::new();
        for _ in 0..n {
            let h = fsys::new().expect("handle");
            let p = tmp("multi_handle");
            let _ = h.write_batch(&[(p.as_path(), b"x".as_slice())]);
            let _ = std::fs::remove_file(&p);
            handles.push(h);
        }
        // All n handles drop here, in declaration order (last in
        // pushed first out of scope). Each runs its own shutdown.
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "{n} handles dropping independently should not exceed 10s, got {elapsed:?}"
    );
}
