//! 0.4.0 integration: 16-thread concurrent batch submission stress test.
//!
//! Verifies the `Pipeline`'s bounded MPMC queue + lazy-spawned dispatcher
//! correctly serialises ops from many producer threads. Ordering is
//! guaranteed *within* a batch (decision #3); across-batch ordering is
//! determined by which producer thread won the queue race, which is
//! intentionally unspecified — the assertion is "every submitted batch
//! is durable and complete," not "submission order across threads."

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_pipe_conc_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

#[test]
fn sixteen_threads_each_writing_distinct_paths_all_complete() {
    let h = Arc::new(fsys::new().expect("handle"));
    let n_threads = 16;
    let writes_per_thread = 8;

    let dir = tmp("conc_root");
    std::fs::create_dir_all(&dir).unwrap();
    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _g = DirGuard(dir.clone());

    let mut handles = Vec::new();
    for t in 0..n_threads {
        let h = Arc::clone(&h);
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            for w in 0..writes_per_thread {
                let path = dir.join(format!("t{t}_w{w}"));
                let payload = format!("t{t}w{w}");
                h.write_batch(&[(path.as_path(), payload.as_bytes())])
                    .expect("write_batch");
            }
        }));
    }
    for h in handles {
        h.join().expect("thread join");
    }

    // Verify every file landed with the expected content.
    for t in 0..n_threads {
        for w in 0..writes_per_thread {
            let path = dir.join(format!("t{t}_w{w}"));
            let expected = format!("t{t}w{w}");
            let actual = std::fs::read_to_string(&path).expect("file should exist");
            assert_eq!(actual, expected, "t{t}_w{w}");
        }
    }
}

#[test]
fn sixteen_threads_submitting_multi_op_batches_maintain_intra_batch_order() {
    // Within each batch, ops execute in submission order (decision #3).
    // Across threads, the dispatcher interleaves but each batch's
    // ops are atomic with respect to ordering. Verify the last-write-
    // wins property holds within each thread's per-path batch.
    let h = Arc::new(fsys::new().expect("handle"));
    let n_threads = 16;

    let dir = tmp("intra_order");
    std::fs::create_dir_all(&dir).unwrap();
    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _g = DirGuard(dir.clone());

    let mut handles = Vec::new();
    for t in 0..n_threads {
        let h = Arc::clone(&h);
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            // Each thread submits a batch with three writes to its own
            // unique file; the last write must win.
            let path = dir.join(format!("t{t}"));
            h.write_batch(&[
                (path.as_path(), b"first".as_slice()),
                (path.as_path(), b"second".as_slice()),
                (path.as_path(), b"final".as_slice()),
            ])
            .expect("write_batch");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    for t in 0..n_threads {
        let path = dir.join(format!("t{t}"));
        let actual = std::fs::read(&path).unwrap();
        assert_eq!(actual, b"final", "thread {t}: last-write-wins");
    }
}

#[test]
fn isolated_handles_do_not_interfere() {
    // Sixteen threads, each with its OWN handle, all writing to
    // overlapping path prefixes. Per decision #6 (per-handle
    // dispatcher), these are independent dispatchers; no cross-handle
    // interference is allowed.
    let n_threads = 16;
    let dir = tmp("isolated");
    std::fs::create_dir_all(&dir).unwrap();
    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _g = DirGuard(dir.clone());

    let mut handles = Vec::new();
    for t in 0..n_threads {
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            let h = fsys::new().expect("per-thread handle");
            let path = dir.join(format!("isolated_t{t}"));
            h.write_batch(&[(path.as_path(), format!("t{t}").as_bytes())])
                .expect("write_batch");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    for t in 0..n_threads {
        let path = dir.join(format!("isolated_t{t}"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("t{t}"),
            "thread {t}'s isolated handle should have written its file"
        );
    }
}
