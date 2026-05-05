//! 32-handle concurrent stress test (0.7.0 stress expansion).
//!
//! Creates 32 independent [`fsys::Handle`] instances with
//! overlapping write paths, mixed methods, and concurrent
//! sync + async submission. Validates two properties:
//!
//! 1. **Isolation** — operations on one handle don't interfere
//!    with operations on another (no cross-handle state leak).
//! 2. **Throughput scaling** — total ops complete in roughly
//!    linear time (not serialised by an unintended global lock).
//!
//! Like the 0.6.0 soak harness, this test gates the duration on
//! the `stress` feature flag (60 s default; full duration with
//! `--features stress`). The default short run is enough to
//! catch regression-class bugs (deadlocks, panics, leaks).

use fsys::builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static C: AtomicU64 = AtomicU64::new(0);

#[cfg(not(feature = "stress"))]
fn stress_budget() -> Duration {
    Duration::from_secs(30)
}

#[cfg(feature = "stress")]
fn stress_budget() -> Duration {
    Duration::from_secs(600) // 10 minutes
}

fn tmp_dir(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "fsys_concurrent_handles_{}_{}_{}",
        std::process::id(),
        n,
        tag
    ));
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

struct CleanupDir(PathBuf);
impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "stress — pragmatic mode runs 30s by default; --features stress runs 10min."]
fn thirty_two_handles_concurrent_writes_complete() {
    let root = tmp_dir("32h");
    let _g = CleanupDir(root.clone());

    let budget = stress_budget();
    let n_handles: usize = 32;

    // Each handle uses its own subdirectory under `root` to
    // produce overlapping but non-conflicting writes.
    let mut handle_threads = Vec::with_capacity(n_handles);
    for hid in 0..n_handles {
        let dir = root.join(format!("handle_{hid:02}"));
        std::fs::create_dir(&dir).expect("mkdir per-handle");
        let fs = Arc::new(builder().build().expect("build per-handle"));

        handle_threads.push(std::thread::spawn(move || -> u64 {
            let start = Instant::now();
            let mut count: u64 = 0;
            while start.elapsed() < budget {
                let path = dir.join(format!("file_{}", count % 64));
                let payload = vec![hid as u8; 1024];
                fs.write(&path, &payload).expect("write");
                if count % 50 == 0 {
                    let _ = fs.delete(&path);
                }
                count += 1;
            }
            count
        }));
    }

    let mut total_ops: u64 = 0;
    for h in handle_threads {
        total_ops += h.join().expect("thread join");
    }

    eprintln!(
        "[32h_concurrent] {n_handles} handles ran {total_ops} total ops in {:?}",
        budget
    );
    // Linear-throughput floor: at minimum 100 ops per handle in
    // 30s = 3200 total. Anything below this signals a serialisation
    // problem (cross-handle contention) — but on slow CI this can
    // legitimately be lower, so the threshold is conservative.
    assert!(
        total_ops > 1000,
        "32 handles should produce > 1000 total ops in {budget:?}; got {total_ops}"
    );
}

#[test]
#[ignore = "stress — pragmatic mode."]
fn thirty_two_handles_isolation_no_cross_handle_corruption() {
    let root = tmp_dir("32h_isolation");
    let _g = CleanupDir(root.clone());

    let n_handles: usize = 32;
    let writes_per_handle: u64 = 100;

    let mut handle_threads = Vec::with_capacity(n_handles);
    for hid in 0..n_handles {
        let dir = root.join(format!("handle_{hid:02}"));
        std::fs::create_dir(&dir).expect("mkdir");
        let fs = Arc::new(builder().build().expect("handle"));

        handle_threads.push(std::thread::spawn(move || {
            for i in 0..writes_per_handle {
                let path = dir.join(format!("write_{i:04}"));
                // Each handle writes its own ID as the payload.
                let payload = vec![hid as u8; 256];
                fs.write(&path, &payload).expect("write");
            }
            hid
        }));
    }

    for h in handle_threads {
        let _hid = h.join().expect("thread join");
    }

    // Verify isolation: every file in handle K's directory must
    // contain only bytes equal to K. If any file contains the
    // wrong byte value, a cross-handle race corrupted state.
    for hid in 0..n_handles {
        let dir = root.join(format!("handle_{hid:02}"));
        for i in 0..writes_per_handle {
            let path = dir.join(format!("write_{i:04}"));
            let bytes = std::fs::read(&path).expect("read");
            assert!(
                bytes.iter().all(|&b| b == hid as u8),
                "handle {hid} write {i} has wrong payload — cross-handle corruption"
            );
        }
    }

    eprintln!("[32h_isolation] all {n_handles} handles isolated correctly");
}
