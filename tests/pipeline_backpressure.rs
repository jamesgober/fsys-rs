//! 0.4.0 integration: backpressure semantics under a small queue.
//!
//! Decision #4 — bounded queue with blocking submission. When the
//! queue is full, calls to `write_batch` etc. **block** until space
//! is available; they do **not** return an error in 0.4.0
//! (`Error::QueueFull` is reserved for a future opt-in mode).
//!
//! Validating "the call blocks" is awkward in a unit test (timing-
//! dependent), so we validate the observable contract instead: with a
//! tiny queue (capacity 1), submitting many batches faster than the
//! dispatcher can drain still produces correct on-disk state for
//! every batch.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_pipe_bp_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

#[test]
fn many_batches_through_capacity_one_queue_all_succeed() {
    // batch_queue_max=1: only one batch at a time can sit in the queue.
    // Producers must therefore block until the dispatcher drains.
    let h = fsys::builder()
        .batch_queue_max(1)
        .build()
        .expect("build with tiny queue");

    let dir = tmp("bp_root");
    std::fs::create_dir_all(&dir).unwrap();
    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _g = DirGuard(dir.clone());

    // Submit 50 single-op batches as fast as we can. With queue
    // capacity 1, most of these will block briefly waiting for the
    // previous one to drain. Every batch must still complete and the
    // file state must be consistent.
    let n = 50;
    for i in 0..n {
        let path = dir.join(format!("bp_{i}"));
        h.write_batch(&[(path.as_path(), format!("v{i}").as_bytes())])
            .expect("backpressure submission must not error");
    }

    for i in 0..n {
        let path = dir.join(format!("bp_{i}"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("v{i}"),
            "every batch's payload must land"
        );
    }
}

#[test]
fn concurrent_producers_through_tiny_queue_all_succeed() {
    let h = Arc::new(
        fsys::builder()
            .batch_queue_max(2)
            .build()
            .expect("build with tiny queue"),
    );

    let dir = tmp("bp_conc_root");
    std::fs::create_dir_all(&dir).unwrap();
    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _g = DirGuard(dir.clone());

    let n_threads = 8;
    let per_thread = 10;
    let mut handles = Vec::new();
    for t in 0..n_threads {
        let h = Arc::clone(&h);
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            for w in 0..per_thread {
                let path = dir.join(format!("bp_t{t}_w{w}"));
                h.write_batch(&[(path.as_path(), b"x".as_slice())])
                    .expect("submission");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    for t in 0..n_threads {
        for w in 0..per_thread {
            let path = dir.join(format!("bp_t{t}_w{w}"));
            assert_eq!(std::fs::read(&path).unwrap(), b"x");
        }
    }
}

#[test]
fn queue_full_does_not_emit_queue_full_error_in_0_4_0() {
    // Reserved variant per decision #4 — explicit assertion that
    // `Error::QueueFull` is never observed in 0.4.0.
    let h = fsys::builder().batch_queue_max(1).build().expect("build");
    let p = tmp("never_queue_full");
    let result = h.write_batch(&[(p.as_path(), b"data".as_slice())]);
    if let Err(ref e) = result {
        match e.inner() {
            fsys::Error::QueueFull => {
                panic!("Error::QueueFull must NOT be emitted in 0.4.0");
            }
            _ => { /* any other variant is acceptable in this scenario */ }
        }
    }
    let _ = std::fs::remove_file(&p);
}
