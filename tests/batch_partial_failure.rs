//! 0.4.0 integration: decision #5 — independent ops, not transactions.
//!
//! When a batch op fails (returned `Err`, not panic), the dispatcher
//! reports `BatchError { failed_at, completed, source }` and stops.
//! - Ops that succeeded before the failure ARE durable.
//! - The failing op is NOT durable.
//! - Subsequent ops in the same batch are NOT attempted.
//! - fsys does NOT roll back successful ops.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_batch_partial_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn failure_reports_correct_failed_at_and_completed() {
    let h = fsys::new().expect("handle");
    let ok1 = tmp("ok_1");
    let ok2 = tmp("ok_2");
    let bad = tmp("bad_dir");
    let never = tmp("never_attempted");
    let _g1 = FileGuard(ok1.clone());
    let _g2 = FileGuard(ok2.clone());
    std::fs::create_dir_all(&bad).unwrap();
    let _bg = DirGuard(bad.clone());
    let _ng = FileGuard(never.clone());

    let result = h.write_batch(&[
        (ok1.as_path(), b"first".as_slice()),
        (ok2.as_path(), b"second".as_slice()),
        (bad.as_path(), b"this-will-fail".as_slice()), // dir, not file
        (never.as_path(), b"never-runs".as_slice()),
    ]);

    let err = result.expect_err("expected failure");
    assert_eq!(err.failed_at, 2, "third op (index 2) should fail");
    assert_eq!(err.completed, 2, "first two ops should have succeeded");
}

#[test]
fn ops_before_failure_are_durable() {
    let h = fsys::new().expect("handle");
    let durable = tmp("durable_before_fail");
    let bad = tmp("fail_target_dir");
    let _g1 = FileGuard(durable.clone());
    std::fs::create_dir_all(&bad).unwrap();
    let _bg = DirGuard(bad.clone());

    let _ = h.write_batch(&[
        (durable.as_path(), b"persisted".as_slice()),
        (bad.as_path(), b"fail".as_slice()),
    ]);

    // The durable file must exist with its content even though the
    // batch as a whole failed.
    assert_eq!(std::fs::read(&durable).unwrap(), b"persisted");
}

#[test]
fn ops_after_failure_are_not_attempted() {
    let h = fsys::new().expect("handle");
    let ok = tmp("after_failure_ok");
    let bad = tmp("middle_fail_dir");
    let after = tmp("after_failure_never");
    let _g1 = FileGuard(ok.clone());
    let _g2 = FileGuard(after.clone());
    std::fs::create_dir_all(&bad).unwrap();
    let _bg = DirGuard(bad.clone());

    let _ = h.write_batch(&[
        (ok.as_path(), b"first".as_slice()),
        (bad.as_path(), b"fails".as_slice()),
        (after.as_path(), b"never-written".as_slice()),
    ]);

    // The "after" file must NOT exist (op was not attempted).
    assert!(!after.exists(), "ops after the failing index must not run");
}

#[test]
fn no_rollback_of_successful_ops() {
    // Decision #5: fsys explicitly does NOT roll back. The first op
    // succeeded; even though the batch overall failed, the file is
    // still on disk. This test is the explicit assertion of that
    // contract — a future "let me add rollback" PR would break this
    // test, surfacing the contract change for review.
    let h = fsys::new().expect("handle");
    let durable = tmp("no_rollback");
    let bad = tmp("no_rollback_bad");
    let _g1 = FileGuard(durable.clone());
    std::fs::create_dir_all(&bad).unwrap();
    let _bg = DirGuard(bad.clone());

    let result = h.write_batch(&[
        (durable.as_path(), b"survives".as_slice()),
        (bad.as_path(), b"fails".as_slice()),
    ]);
    assert!(result.is_err());

    // Explicit no-rollback assertion.
    assert!(durable.exists(), "successful ops must NOT be rolled back");
    assert_eq!(std::fs::read(&durable).unwrap(), b"survives");
}

#[test]
fn batch_error_inner_carries_underlying_error() {
    let h = fsys::new().expect("handle");
    let bad = tmp("inner_error_dir");
    std::fs::create_dir_all(&bad).unwrap();
    let _bg = DirGuard(bad.clone());

    let result = h.write_batch(&[(bad.as_path(), b"x".as_slice())]);
    let err = result.expect_err("expected failure");
    // The inner error should be a real fsys::Error variant, not just
    // the BatchError struct itself. Pattern matching here proves the
    // inner type is well-formed.
    match err.inner() {
        fsys::Error::AtomicReplaceFailed { .. } => { /* expected */ }
        fsys::Error::Io(_) => { /* also acceptable */ }
        other => panic!("unexpected inner error variant: {other:?}"),
    }
}
