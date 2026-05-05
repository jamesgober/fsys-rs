//! Integration tests for the 0.6.0 async batch layer.
//!
//! Validates that `Handle::write_batch_async` / `delete_batch_async`
//! / `copy_batch_async` route through the per-handle dispatcher
//! exactly like sync batches, but respond via `tokio::sync::oneshot`
//! per locked decision D-5.

#![cfg(feature = "async")]

use fsys::builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_async_batch_{}_{}_{}",
        std::process::id(),
        n,
        tag
    ))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn write_batch_async_persists_all_payloads() {
    let p1 = tmp_path("wb1");
    let p2 = tmp_path("wb2");
    let p3 = tmp_path("wb3");
    let _g1 = Cleanup(p1.clone());
    let _g2 = Cleanup(p2.clone());
    let _g3 = Cleanup(p3.clone());

    let fs = Arc::new(builder().build().expect("handle"));

    fs.clone()
        .write_batch_async(vec![
            (p1.clone(), b"first".to_vec()),
            (p2.clone(), b"second".to_vec()),
            (p3.clone(), b"third".to_vec()),
        ])
        .await
        .expect("write_batch_async");

    assert_eq!(std::fs::read(&p1).unwrap(), b"first");
    assert_eq!(std::fs::read(&p2).unwrap(), b"second");
    assert_eq!(std::fs::read(&p3).unwrap(), b"third");
}

#[tokio::test]
async fn delete_batch_async_removes_all() {
    let p1 = tmp_path("db1");
    let p2 = tmp_path("db2");
    std::fs::write(&p1, b"x").unwrap();
    std::fs::write(&p2, b"y").unwrap();

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .delete_batch_async(vec![p1.clone(), p2.clone()])
        .await
        .expect("delete_batch_async");

    assert!(!p1.exists());
    assert!(!p2.exists());
}

#[tokio::test]
async fn copy_batch_async_duplicates_each_pair() {
    let s1 = tmp_path("cb_src1");
    let s2 = tmp_path("cb_src2");
    let d1 = tmp_path("cb_dst1");
    let d2 = tmp_path("cb_dst2");
    let _g_s1 = Cleanup(s1.clone());
    let _g_s2 = Cleanup(s2.clone());
    let _g_d1 = Cleanup(d1.clone());
    let _g_d2 = Cleanup(d2.clone());

    std::fs::write(&s1, b"alpha").unwrap();
    std::fs::write(&s2, b"beta").unwrap();

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .copy_batch_async(vec![(s1.clone(), d1.clone()), (s2.clone(), d2.clone())])
        .await
        .expect("copy_batch_async");

    assert_eq!(std::fs::read(&d1).unwrap(), b"alpha");
    assert_eq!(std::fs::read(&d2).unwrap(), b"beta");
}

#[tokio::test]
async fn empty_batch_async_completes_immediately() {
    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_batch_async(Vec::new())
        .await
        .expect("empty batch ok");
}

#[tokio::test]
async fn batch_async_failure_reports_failed_at_correctly() {
    // First op succeeds, second fails (path escape rejected by
    // resolve_path on a rooted handle), third never attempted.
    let root = std::env::temp_dir().join(format!("fsys_async_batch_root_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let _g = scopeguard_remove_dir_all(root.clone());

    let fs = Arc::new(builder().root(root.clone()).build().expect("rooted handle"));
    let err = fs
        .clone()
        .write_batch_async(vec![
            (PathBuf::from("ok.dat"), b"first".to_vec()),
            (PathBuf::from("../escape.dat"), b"second".to_vec()),
            (PathBuf::from("never.dat"), b"third".to_vec()),
        ])
        .await
        .expect_err("path escape must fail");
    assert_eq!(err.failed_at, 1);
}

fn scopeguard_remove_dir_all(p: PathBuf) -> impl Drop {
    struct G(PathBuf);
    impl Drop for G {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    G(p)
}
