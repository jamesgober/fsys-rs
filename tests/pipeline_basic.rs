//! 0.4.0 integration: basic batch durability through the public API.
//!
//! Verifies that `write_batch`, `delete_batch`, and `copy_batch` flow
//! through the group lane and produce the expected on-disk results.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_pipe_basic_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

struct Cleanup(Vec<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

#[test]
fn write_batch_creates_every_file() {
    let h = fsys::new().expect("handle");
    let p1 = tmp("w1");
    let p2 = tmp("w2");
    let p3 = tmp("w3");
    let _g = Cleanup(vec![p1.clone(), p2.clone(), p3.clone()]);

    h.write_batch(&[
        (p1.as_path(), b"alpha".as_slice()),
        (p2.as_path(), b"beta".as_slice()),
        (p3.as_path(), b"gamma".as_slice()),
    ])
    .expect("write_batch");

    assert_eq!(std::fs::read(&p1).unwrap(), b"alpha");
    assert_eq!(std::fs::read(&p2).unwrap(), b"beta");
    assert_eq!(std::fs::read(&p3).unwrap(), b"gamma");
}

#[test]
fn delete_batch_removes_every_file() {
    let h = fsys::new().expect("handle");
    let p1 = tmp("d1");
    let p2 = tmp("d2");
    std::fs::write(&p1, b"x").unwrap();
    std::fs::write(&p2, b"y").unwrap();
    let _g = Cleanup(vec![p1.clone(), p2.clone()]);

    h.delete_batch(&[p1.as_path(), p2.as_path()])
        .expect("delete_batch");

    assert!(!p1.exists());
    assert!(!p2.exists());
}

#[test]
fn delete_batch_is_idempotent_on_missing_files() {
    let h = fsys::new().expect("handle");
    let missing = tmp("missing");
    h.delete_batch(&[missing.as_path()])
        .expect("missing file should not fail delete_batch");
}

#[test]
fn copy_batch_duplicates_every_pair() {
    let h = fsys::new().expect("handle");
    let s1 = tmp("cs1");
    let s2 = tmp("cs2");
    let d1 = tmp("cd1");
    let d2 = tmp("cd2");
    std::fs::write(&s1, b"src1").unwrap();
    std::fs::write(&s2, b"src2").unwrap();
    let _g = Cleanup(vec![s1.clone(), s2.clone(), d1.clone(), d2.clone()]);

    h.copy_batch(&[(s1.as_path(), d1.as_path()), (s2.as_path(), d2.as_path())])
        .expect("copy_batch");

    assert_eq!(std::fs::read(&d1).unwrap(), b"src1");
    assert_eq!(std::fs::read(&d2).unwrap(), b"src2");
}

#[test]
fn empty_batch_succeeds_immediately() {
    let h = fsys::new().expect("handle");
    let empty: &[(&std::path::Path, &[u8])] = &[];
    h.write_batch(empty).expect("empty");
    let empty_d: &[&std::path::Path] = &[];
    h.delete_batch(empty_d).expect("empty delete");
    let empty_c: &[(&std::path::Path, &std::path::Path)] = &[];
    h.copy_batch(empty_c).expect("empty copy");
}

#[test]
fn solo_writes_and_batch_writes_share_storage() {
    // Verifies the solo lane and group lane both produce identical
    // observable file state. This confirms the dispatcher's
    // atomic-replace helper is consistent with crud::file::write.
    let h = fsys::new().expect("handle");
    let solo = tmp("solo");
    let group = tmp("group");
    let _g = Cleanup(vec![solo.clone(), group.clone()]);

    h.write(&solo, b"identical").expect("solo write");
    h.write_batch(&[(group.as_path(), b"identical".as_slice())])
        .expect("batch write");

    assert_eq!(
        std::fs::read(&solo).unwrap(),
        std::fs::read(&group).unwrap()
    );
}
