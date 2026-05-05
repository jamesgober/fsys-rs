//! Path-escape rejection coverage for `Builder::root`-scoped handles.
//!
//! The hostile-input matrix below is intentionally exhaustive
//! because path security is the most attack-relevant surface in
//! the public API, and the J-checkpoint security review will
//! re-validate this with a fresh subagent. Every entry that the
//! handle-root scope rejects must continue to be rejected; every
//! entry that resolves cleanly must continue to resolve cleanly.

use fsys::{builder, Error};
use std::path::PathBuf;

fn sandbox(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("fsys_pathsec_{}_{}", std::process::id(), tag));
    std::fs::create_dir_all(&p).expect("mkdir sandbox");
    p
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn parent_traversal_dotdot_rejected() {
    let root = sandbox("dotdot");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");

    let err = fs
        .write("../escaped.txt", b"x")
        .expect_err("../ should be rejected");
    assert!(matches!(err, Error::InvalidPath { .. }));
}

#[test]
fn nested_dotdot_chain_rejected() {
    let root = sandbox("nested_dotdot");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");

    let err = fs
        .write("subdir/../../escaped.txt", b"x")
        .expect_err("subdir/../../ should be rejected");
    assert!(matches!(err, Error::InvalidPath { .. }));
}

#[test]
fn absolute_path_outside_root_rejected() {
    let root = sandbox("absolute");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");

    // Absolute path that points outside the handle's root.
    #[cfg(unix)]
    let outside = "/tmp/totally_outside_the_root.txt";
    #[cfg(windows)]
    let outside = "C:\\totally_outside_the_root.txt";

    let err = fs
        .write(outside, b"x")
        .expect_err("absolute outside root rejected");
    assert!(matches!(err, Error::InvalidPath { .. }));
}

#[test]
fn relative_path_inside_root_accepted() {
    let root = sandbox("inside");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");

    fs.write("note.txt", b"x")
        .expect("relative inside root must succeed");
    let _ = fs.write("sub/nested.txt", b"y"); // missing parent — error path
    fs.mkdir_all("sub").expect("mkdir sub");
    fs.write("sub/nested.txt", b"y").expect("nested write");
}

#[test]
fn dotdot_inside_path_that_resolves_inside_root_accepted() {
    // `a/b/../c.txt` resolves to `a/c.txt` — entirely inside root.
    let root = sandbox("inside_dotdot");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");
    fs.mkdir_all("a/b").expect("mkdir a/b");

    // Per the audit, the current `find` rejects ANY pattern with
    // `..` even if it resolves inside the base. For `write`, the
    // resolution is checked against the canonicalised root, so
    // `a/b/../c.txt` IS accepted (resolves to `a/c.txt`). This
    // test pins that distinction.
    fs.write("a/b/../c.txt", b"resolved")
        .expect("a/b/../c.txt resolves to a/c.txt — accepted");
}

#[test]
fn empty_relative_path_handled_gracefully() {
    let root = sandbox("empty_path");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");

    // Empty relative path — implementation-defined. The contract
    // is "no panic, no UB, error is fine." Pin that contract.
    let result = fs.write("", b"x");
    assert!(
        result.is_err(),
        "empty path should error, not produce undefined behaviour"
    );
}

// Find pattern security — `find` rejects any pattern with `..`
// even if it resolves inside the base.
#[test]
fn find_pattern_with_dotdot_rejected() {
    let root = sandbox("find_dotdot");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");

    let err = fs
        .find(&root, "../*.txt")
        .expect_err("find with .. must reject");
    assert!(matches!(err, Error::InvalidPath { .. }));
}

#[test]
fn find_absolute_pattern_rejected() {
    let root = sandbox("find_abs");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");

    #[cfg(unix)]
    let pattern = "/etc/*";
    #[cfg(windows)]
    let pattern = "C:\\Windows\\*";

    let err = fs
        .find(&root, pattern)
        .expect_err("absolute pattern rejected");
    assert!(matches!(err, Error::InvalidPath { .. }));
}

// 0.8.0 J — symlink-escape regression test.
//
// The pre-0.8.0 lexical-only `resolve_path` allowed an attacker to
// place a symlink inside the root pointing outside, and writes
// "through" the symlink would silently escape. The 0.8.0 J
// canonical-prefix check rejects.
//
// Windows symlink creation requires admin or developer mode, so
// this test is Unix-only. The Windows path is exercised via the
// `Builder::root` canonicalisation test in `tests/path_security.rs`.
#[cfg(unix)]
#[test]
fn symlink_escape_rejected_via_canonical_prefix_check() {
    use std::os::unix::fs::symlink;

    let root = sandbox("symlink_escape");
    let _g = Cleanup(root.clone());

    // Create a victim file *outside* the root.
    let outside = std::env::temp_dir().join(format!(
        "fsys_symlink_victim_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&outside, b"original outside contents").expect("seed outside");

    // Create a symlink INSIDE the root pointing to the outside file.
    let link = root.join("evil_link.txt");
    symlink(&outside, &link).expect("create symlink");

    let fs = builder().root(&root).build().expect("handle");

    // Pre-fix: this would write through the symlink and clobber
    // the outside file. Post-fix: rejected with InvalidPath.
    let result = fs.write("evil_link.txt", b"PWNED");
    let _ = std::fs::remove_file(&outside);
    assert!(
        matches!(result, Err(Error::InvalidPath { .. })),
        "symlink-through-root must be rejected; got {result:?}"
    );
}

// 0.8.0 J — brace-expansion bound regression.
#[test]
fn find_pathological_brace_pattern_does_not_explode() {
    // {a,b}^20 = 1M expansions if unbounded. Bound is 1024.
    // The test should complete in <100ms (no allocator melt).
    let root = sandbox("brace_bomb");
    let _g = Cleanup(root.clone());
    let fs = builder().root(&root).build().expect("handle");

    let pathological = "{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}";
    let start = std::time::Instant::now();
    let _ = fs.find(&root, pathological); // result irrelevant; we care about time
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "pathological brace pattern should be bounded; took {elapsed:?}"
    );
}
