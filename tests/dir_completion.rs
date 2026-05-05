//! Integration tests for the 0.6.0 directory completion API:
//! [`Handle::scan`], [`Handle::find`], and [`Handle::count`].

use fsys::builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "fsys_dir_completion_{}_{}_{}",
        std::process::id(),
        n,
        tag
    ));
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn scan_non_recursive_returns_only_immediate_children() {
    let root = tmp_dir("scan_flat");
    let _g = Cleanup(root.clone());

    std::fs::write(root.join("a.txt"), b"x").unwrap();
    std::fs::write(root.join("b.txt"), b"x").unwrap();
    std::fs::create_dir(root.join("subdir")).unwrap();
    std::fs::write(root.join("subdir/c.txt"), b"x").unwrap();

    let fs = builder().build().expect("handle");
    let entries = fs.scan(&root).expect("scan");
    assert_eq!(entries.len(), 3, "non-recursive should see 3 entries");
}

#[test]
fn scan_recursive_descends_into_subdirectories() {
    let root = tmp_dir("scan_deep");
    let _g = Cleanup(root.clone());

    std::fs::write(root.join("a.txt"), b"x").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/b.txt"), b"x").unwrap();
    std::fs::create_dir(root.join("sub/deeper")).unwrap();
    std::fs::write(root.join("sub/deeper/c.txt"), b"x").unwrap();

    let fs = builder().build().expect("handle");
    let entries = fs.scan_all(&root).expect("scan_all");
    // 3 files + 2 directories = 5 entries.
    assert_eq!(entries.len(), 5, "recursive should see 5 entries");
}

#[test]
fn scan_empty_directory_returns_empty_vec() {
    let root = tmp_dir("scan_empty");
    let _g = Cleanup(root.clone());

    let fs = builder().build().expect("handle");
    let entries = fs.scan_all(&root).expect("scan_all");
    assert!(entries.is_empty());
}

#[test]
fn scan_missing_path_returns_io_error() {
    let root = tmp_dir("scan_missing");
    drop(Cleanup(root.clone())); // remove immediately
    let _ = std::fs::remove_dir_all(&root);

    let fs = builder().build().expect("handle");
    let err = fs.scan(&root).expect_err("missing path");
    matches!(err, fsys::Error::Io(_));
}

#[test]
fn find_glob_star_matches_immediate_children() {
    let root = tmp_dir("find_star");
    let _g = Cleanup(root.clone());

    std::fs::write(root.join("foo.log"), b"x").unwrap();
    std::fs::write(root.join("bar.log"), b"x").unwrap();
    std::fs::write(root.join("baz.txt"), b"x").unwrap();

    let fs = builder().build().expect("handle");
    let mut hits = fs.find(&root, "*.log").expect("find");
    hits.sort();
    assert_eq!(hits.len(), 2);
    assert!(hits[0].file_name().unwrap() == "bar.log");
    assert!(hits[1].file_name().unwrap() == "foo.log");
}

#[test]
fn find_glob_double_star_recurses() {
    let root = tmp_dir("find_recursive");
    let _g = Cleanup(root.clone());

    std::fs::write(root.join("top.log"), b"x").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/inner.log"), b"x").unwrap();
    std::fs::create_dir(root.join("sub/deep")).unwrap();
    std::fs::write(root.join("sub/deep/leaf.log"), b"x").unwrap();
    std::fs::write(root.join("sub/deep/notes.txt"), b"x").unwrap();

    let fs = builder().build().expect("handle");
    let hits = fs.find(&root, "**/*.log").expect("find");
    assert_eq!(hits.len(), 3, "should match every .log file in tree");
}

#[test]
fn find_no_match_returns_empty() {
    let root = tmp_dir("find_empty");
    let _g = Cleanup(root.clone());

    std::fs::write(root.join("a.txt"), b"x").unwrap();

    let fs = builder().build().expect("handle");
    let hits = fs.find(&root, "*.nonexistent").expect("find");
    assert!(hits.is_empty());
}

#[test]
fn find_alternation_matches_either_pattern() {
    let root = tmp_dir("find_alt");
    let _g = Cleanup(root.clone());

    std::fs::write(root.join("nginx.conf"), b"x").unwrap();
    std::fs::write(root.join("apache.conf"), b"x").unwrap();
    std::fs::write(root.join("other.txt"), b"x").unwrap();

    let fs = builder().build().expect("handle");
    let hits = fs.find(&root, "{nginx,apache}*.conf").expect("find");
    assert_eq!(hits.len(), 2);
}

#[test]
fn find_rejects_pattern_that_escapes_base() {
    let root = tmp_dir("find_escape");
    let _g = Cleanup(root.clone());
    let fs = builder().build().expect("handle");
    let err = fs
        .find(&root, "../../etc/passwd")
        .expect_err("escape rejected");
    matches!(err, fsys::Error::InvalidPath { .. });
}

#[test]
fn count_non_recursive_skips_subdirectories() {
    let root = tmp_dir("count_flat");
    let _g = Cleanup(root.clone());

    std::fs::write(root.join("a.txt"), b"x").unwrap();
    std::fs::write(root.join("b.txt"), b"x").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/inner.txt"), b"x").unwrap();

    let fs = builder().build().expect("handle");
    assert_eq!(fs.count(&root).expect("count"), 2);
}

#[test]
fn count_recursive_includes_descendants() {
    let root = tmp_dir("count_deep");
    let _g = Cleanup(root.clone());

    std::fs::write(root.join("a.txt"), b"x").unwrap();
    std::fs::create_dir(root.join("s1")).unwrap();
    std::fs::write(root.join("s1/b.txt"), b"x").unwrap();
    std::fs::write(root.join("s1/c.txt"), b"x").unwrap();
    std::fs::create_dir(root.join("s2")).unwrap();
    std::fs::write(root.join("s2/d.txt"), b"x").unwrap();

    let fs = builder().build().expect("handle");
    assert_eq!(fs.count_all(&root).expect("count_all"), 4);
}

#[test]
fn count_empty_directory_returns_zero() {
    let root = tmp_dir("count_empty");
    let _g = Cleanup(root.clone());
    let fs = builder().build().expect("handle");
    assert_eq!(fs.count_all(&root).expect("count_all"), 0);
}
