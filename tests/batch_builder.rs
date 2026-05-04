//! 0.4.0 integration: `Batch` builder API.
//!
//! Exercises the chainable builder pattern and confirms it produces
//! identical observable results to the slice-based `write_batch` /
//! `delete_batch` / `copy_batch` methods on [`fsys::Handle`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_batch_builder_{}_{}_{}",
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

#[test]
fn empty_batch_commits_successfully() {
    let h = fsys::new().expect("handle");
    let b = h.batch();
    assert!(b.is_empty());
    assert_eq!(b.len(), 0);
    b.commit().expect("empty commit");
}

#[test]
fn chained_writes_persist_in_order() {
    let h = fsys::new().expect("handle");
    let p = tmp("chained");
    let _g = FileGuard(p.clone());

    let mut b = h.batch();
    let _ = b
        .write(&p, b"first")
        .write(&p, b"second")
        .write(&p, b"third");
    assert_eq!(b.len(), 3);
    b.commit().expect("commit");

    // Strict input order → last-write-wins.
    assert_eq!(std::fs::read(&p).unwrap(), b"third");
}

#[test]
fn mixed_op_chain_executes_each_op_correctly() {
    let h = fsys::new().expect("handle");
    let written = tmp("mixed_w");
    let copy_src = tmp("mixed_cs");
    let copy_dst = tmp("mixed_cd");
    let to_delete = tmp("mixed_d");
    let _g1 = FileGuard(written.clone());
    let _g2 = FileGuard(copy_src.clone());
    let _g3 = FileGuard(copy_dst.clone());
    let _g4 = FileGuard(to_delete.clone());

    std::fs::write(&copy_src, b"src-data").unwrap();
    std::fs::write(&to_delete, b"existing").unwrap();

    let mut b = h.batch();
    let _ = b
        .write(&written, b"hello")
        .copy(&copy_src, &copy_dst)
        .delete(&to_delete);
    assert_eq!(b.len(), 3);
    b.commit().expect("commit");

    assert_eq!(std::fs::read(&written).unwrap(), b"hello");
    assert_eq!(std::fs::read(&copy_dst).unwrap(), b"src-data");
    assert!(!to_delete.exists());
}

#[test]
fn slice_api_and_builder_produce_identical_results() {
    let h = fsys::new().expect("handle");
    let slice_target = tmp("equiv_slice");
    let builder_target = tmp("equiv_builder");
    let _g1 = FileGuard(slice_target.clone());
    let _g2 = FileGuard(builder_target.clone());

    h.write_batch(&[(slice_target.as_path(), b"identical".as_slice())])
        .expect("slice batch");

    let mut b = h.batch();
    let _ = b.write(&builder_target, b"identical");
    b.commit().expect("builder batch");

    assert_eq!(
        std::fs::read(&slice_target).unwrap(),
        std::fs::read(&builder_target).unwrap()
    );
}

#[test]
fn builder_must_use_attribute_prevents_silent_drop() {
    // Compile-time check: `Batch` is `#[must_use]`. Calling
    // `h.batch()` without binding or commiting would warn. We can't
    // assert the warning at runtime, but we can confirm that the
    // explicit-drop pattern compiles cleanly.
    let h = fsys::new().expect("handle");
    let b = h.batch();
    drop(b);
}

#[test]
fn large_dynamic_batch_succeeds() {
    let h = fsys::new().expect("handle");
    let dir = tmp("large_root");
    std::fs::create_dir_all(&dir).unwrap();
    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _g = DirGuard(dir.clone());

    let n = 200;
    let mut b = h.batch();
    for i in 0..n {
        let path = dir.join(format!("large_{i}"));
        let _ = b.write(&path, format!("v{i}").as_bytes());
    }
    assert_eq!(b.len(), n);
    b.commit().expect("commit large");

    for i in 0..n {
        let path = dir.join(format!("large_{i}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("v{i}"));
    }
}
