//! 0.4.0 integration: strict input-order semantics (decision #3).
//!
//! Within a single batch, ops execute in submission order. The
//! dispatcher does not reorder, coalesce, or deduplicate. This file
//! exercises ordering through the public API across mixed op types.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_batch_order_{}_{}_{}",
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
fn write_batch_repeats_to_same_file_last_one_wins() {
    let h = fsys::new().expect("handle");
    let p = tmp("repeat");
    let _g = FileGuard(p.clone());

    h.write_batch(&[
        (p.as_path(), b"v1".as_slice()),
        (p.as_path(), b"v2".as_slice()),
        (p.as_path(), b"v3".as_slice()),
        (p.as_path(), b"final".as_slice()),
    ])
    .expect("batch");

    assert_eq!(std::fs::read(&p).unwrap(), b"final");
}

#[test]
fn write_then_delete_then_write_in_same_batch_executes_in_order() {
    let h = fsys::new().expect("handle");
    let p = tmp("write_delete_write");
    let _g = FileGuard(p.clone());

    let mut b = h.batch();
    let _ = b
        .write(&p, b"initial")
        .delete(&p)
        .write(&p, b"after-delete");
    b.commit().expect("commit");

    // Final state: file exists with the post-delete payload, because
    // the delete happened before the second write within the batch.
    assert_eq!(std::fs::read(&p).unwrap(), b"after-delete");
}

#[test]
fn delete_after_write_in_same_batch_leaves_file_gone() {
    let h = fsys::new().expect("handle");
    let p = tmp("write_then_delete");
    let _g = FileGuard(p.clone());

    let mut b = h.batch();
    let _ = b.write(&p, b"created").delete(&p);
    b.commit().expect("commit");

    assert!(!p.exists(), "delete after write must remove the file");
}

#[test]
fn copy_observes_post_write_state_within_same_batch() {
    let h = fsys::new().expect("handle");
    let src = tmp("ord_src");
    let dst = tmp("ord_dst");
    let _g1 = FileGuard(src.clone());
    let _g2 = FileGuard(dst.clone());

    let mut b = h.batch();
    let _ = b.write(&src, b"new-content").copy(&src, &dst);
    b.commit().expect("commit");

    // Strict order: write happened before copy, so the copy must see
    // the new content.
    assert_eq!(std::fs::read(&dst).unwrap(), b"new-content");
}

#[test]
fn ten_distinct_writes_in_one_batch_all_persist_in_order() {
    let h = fsys::new().expect("handle");
    let dir = tmp("ten_root");
    std::fs::create_dir_all(&dir).unwrap();
    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _g = DirGuard(dir.clone());

    // Build a batch of 10 writes to distinct files, each carrying its
    // index. After commit, every file must reflect its index.
    let mut b = h.batch();
    let paths: Vec<PathBuf> = (0..10).map(|i| dir.join(format!("ord_{i}"))).collect();
    for (i, p) in paths.iter().enumerate() {
        let _ = b.write(p, format!("v{i}").as_bytes());
    }
    b.commit().expect("commit");

    for (i, p) in paths.iter().enumerate() {
        assert_eq!(std::fs::read_to_string(p).unwrap(), format!("v{i}"));
    }
}
