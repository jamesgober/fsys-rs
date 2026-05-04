//! 0.4.0 integration: post-failure recovery contract.
//!
//! Per decision D-6 in `.dev/DECISIONS-0.4.0.md`, the dispatcher's
//! `catch_unwind` panic-safety wrapper is **unit-tested** in
//! `pipeline::group::tests::test_process_jobs_with_catches_panic_*`
//! using a panicking executor closure passed to the test-only generic
//! variant `process_jobs_with`. **No test-only code lives in the
//! production dispatch path** — this is the architecturally correct
//! way to test panic handling in dispatcher patterns and is the
//! convention used by `tokio`, `crossbeam`, `rayon`, etc.
//!
//! This integration file does **not** inject panics. It validates
//! the public-API contract a user actually cares about: *if a batch
//! fails for whatever reason, my next batch still works*. The
//! failure mechanism here is real platform-level errors (writes
//! aimed at a path that's a directory), not synthetic panics, but
//! the recovery contract being tested is identical.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_pipe_recovery_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn handle_recovers_after_failed_batch() {
    let h = fsys::new().expect("handle");

    // Set up a path that will reject a write (it's a directory).
    let bad = tmp("recover_bad");
    std::fs::create_dir_all(&bad).unwrap();
    let _bg = DirGuard(bad.clone());

    let good = tmp("recover_good");
    let _gg = FileGuard(good.clone());

    // First batch: fails on the bad path.
    let r1 = h.write_batch(&[(bad.as_path(), b"x".as_slice())]);
    assert!(r1.is_err(), "writing to a directory must fail");

    // Second batch on the SAME handle: must succeed. This proves the
    // dispatcher survived the prior failure.
    h.write_batch(&[(good.as_path(), b"recovery".as_slice())])
        .expect("post-failure batch must succeed");
    assert_eq!(std::fs::read(&good).unwrap(), b"recovery");
}

#[test]
fn handle_recovers_after_many_alternating_batches() {
    let h = fsys::new().expect("handle");

    let bad = tmp("alt_bad");
    std::fs::create_dir_all(&bad).unwrap();
    let _bg = DirGuard(bad.clone());

    let mut good_paths = Vec::new();
    for i in 0..10 {
        // Failed batch (target is a directory).
        let bad_result = h.write_batch(&[(bad.as_path(), b"fail".as_slice())]);
        assert!(bad_result.is_err(), "iteration {i}: bad batch must fail");

        // Successful batch.
        let good = tmp(&format!("alt_good_{i}"));
        good_paths.push(good.clone());
        h.write_batch(&[(good.as_path(), format!("g{i}").as_bytes())])
            .expect("good batch after failure");
    }

    for (i, p) in good_paths.iter().enumerate() {
        assert_eq!(
            std::fs::read_to_string(p).unwrap(),
            format!("g{i}"),
            "good batch {i} should be durable"
        );
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn concurrent_batches_after_failure_continue_to_succeed() {
    use std::sync::Arc;
    let h = Arc::new(fsys::new().expect("handle"));

    let bad = tmp("conc_recover_bad");
    std::fs::create_dir_all(&bad).unwrap();
    let _bg = DirGuard(bad.clone());

    // Trigger one failure to "warm up" the failure path.
    let _ = h.write_batch(&[(bad.as_path(), b"fail".as_slice())]);

    let dir = tmp("conc_recover_root");
    std::fs::create_dir_all(&dir).unwrap();
    let _dg = DirGuard(dir.clone());

    let n_threads = 8;
    let mut handles = Vec::new();
    for t in 0..n_threads {
        let h = Arc::clone(&h);
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            for w in 0..4 {
                let path = dir.join(format!("conc_t{t}_w{w}"));
                h.write_batch(&[(path.as_path(), format!("ok_t{t}w{w}").as_bytes())])
                    .expect("post-failure concurrent submission");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    for t in 0..n_threads {
        for w in 0..4 {
            let path = dir.join(format!("conc_t{t}_w{w}"));
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                format!("ok_t{t}w{w}"),
            );
        }
    }
}
