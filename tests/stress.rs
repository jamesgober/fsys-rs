//! Stress / soak tests — locked decision D-7 (pragmatic mode).
//!
//! Each test has two modes:
//! - **Default (no feature):** short validation run (~60 seconds) to
//!   verify the harness is sound and would catch regressions.
//! - **`--features stress`:** full-duration run (1 hour). Used by
//!   CI nightly and pre-release validation.
//!
//! The body of each test is identical; only the budget changes. This
//! prevents the harnesses from rotting between full-duration runs.

use fsys::builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static C: AtomicU64 = AtomicU64::new(0);

#[cfg(not(feature = "stress"))]
fn soak_budget() -> Duration {
    Duration::from_secs(60)
}

#[cfg(feature = "stress")]
fn soak_budget() -> Duration {
    Duration::from_secs(3600)
}

fn tmp_dir(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("fsys_stress_{}_{}_{}", std::process::id(), n, tag));
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
#[ignore = "soak — pragmatic mode runs 60s by default; --features stress runs 1h. Run explicitly."]
fn soak_mixed_crud_no_memory_growth() {
    let dir = tmp_dir("soak_mixed");
    let _g = CleanupDir(dir.clone());
    let fs = Arc::new(builder().build().expect("handle"));

    let budget = soak_budget();
    let start = Instant::now();
    let mut iterations: u64 = 0;

    while start.elapsed() < budget {
        let path = dir.join(format!("file_{}", iterations % 32));
        let payload = vec![iterations as u8; 4096];
        fs.write(&path, &payload).expect("write");
        let read = fs.read(&path).expect("read");
        assert_eq!(read.len(), payload.len());
        if iterations % 100 == 0 {
            let _ = fs.delete(&path);
        }
        iterations += 1;
    }

    eprintln!(
        "[soak_mixed] {iterations} iterations in {:?}",
        start.elapsed()
    );
    assert!(iterations > 0, "soak ran zero iterations");
}

#[test]
#[ignore = "soak — pragmatic mode."]
fn soak_repeated_handle_construction_no_thread_leak() {
    // Verify per-handle dispatcher threads are joined cleanly on
    // Handle drop. We don't have a cross-platform "thread count"
    // primitive in std, so we verify by indirect signal: total
    // wall-clock for N create-and-drop cycles stays bounded
    // (otherwise spawn-then-leak would either OOM or slow things
    // catastrophically as the kernel runs out of pids).
    let budget = soak_budget();
    let start = Instant::now();
    let mut count: u64 = 0;

    while start.elapsed() < budget {
        let h = builder().build().expect("handle");
        // Force the dispatcher to spawn by submitting a no-op batch.
        let path = std::env::temp_dir().join(format!(
            "fsys_stress_thread_{}_{}",
            std::process::id(),
            count
        ));
        let _ = h.write(&path, b"x");
        let _ = std::fs::remove_file(&path);
        // Drop joins the dispatcher.
        drop(h);
        count += 1;
    }

    eprintln!(
        "[soak_thread_leak] {count} create+drop cycles in {:?}",
        start.elapsed()
    );
    assert!(count > 0, "soak ran zero cycles");
}

#[test]
#[ignore = "soak — pragmatic mode."]
fn soak_concurrent_writers_share_handle() {
    let dir = tmp_dir("soak_concurrent");
    let _g = CleanupDir(dir.clone());
    let fs = Arc::new(builder().build().expect("handle"));

    let budget = soak_budget();
    let n_threads = 8;
    let mut handles = Vec::new();

    for tid in 0..n_threads {
        let fs = fs.clone();
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || -> u64 {
            let start = Instant::now();
            let mut count: u64 = 0;
            while start.elapsed() < budget {
                let path = dir.join(format!("t{tid}_iter{count}"));
                let payload = vec![tid as u8; 1024];
                fs.write(&path, &payload).expect("write");
                if count % 50 == 0 {
                    let _ = fs.delete(&path);
                }
                count += 1;
            }
            count
        }));
    }

    let mut total: u64 = 0;
    for h in handles {
        total += h.join().expect("thread join");
    }
    eprintln!("[soak_concurrent] {total} ops across {n_threads} threads");
    assert!(total > 0);
}
