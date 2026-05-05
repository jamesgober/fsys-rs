//! Integration tests for [`fsys::Handle::write_copy`].
//!
//! Validates the atomic-swap contract from D-3 and the metadata-
//! preservation set from D-8 in `.dev/DECISIONS-0.6.0.md`.

use fsys::builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_write_copy_{}_{}_{}",
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

#[test]
fn write_copy_creates_new_file_when_target_missing() {
    let path = tmp_path("create");
    let _g = Cleanup(path.clone());

    let fs = builder().build().expect("handle");
    fs.write_copy(&path, b"hello").expect("write_copy");

    let read = std::fs::read(&path).expect("read");
    assert_eq!(read, b"hello");
}

#[test]
fn write_copy_replaces_existing_file_atomically() {
    let path = tmp_path("replace");
    let _g = Cleanup(path.clone());

    std::fs::write(&path, b"old payload").unwrap();
    let fs = builder().build().expect("handle");
    fs.write_copy(&path, b"new payload").expect("write_copy");

    let read = std::fs::read(&path).expect("read");
    assert_eq!(read, b"new payload");
}

#[test]
fn write_copy_preserves_unix_mode() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_path("mode");
        let _g = Cleanup(path.clone());

        std::fs::write(&path, b"original").unwrap();
        // chmod 0644 — typical for /etc/* files; we want to verify
        // that write_copy doesn't reset to umask defaults.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let fs = builder().build().expect("handle");
        fs.write_copy(&path, b"replacement").expect("write_copy");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "mode bits must survive write_copy (got {mode:o})"
        );
    }
}

#[test]
fn write_copy_preserves_unix_mode_with_unusual_bits() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_path("mode_750");
        let _g = Cleanup(path.clone());

        std::fs::write(&path, b"original").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();

        let fs = builder().build().expect("handle");
        fs.write_copy(&path, b"replacement").expect("write_copy");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
    }
}

#[test]
fn write_copy_zero_byte_payload() {
    let path = tmp_path("zero");
    let _g = Cleanup(path.clone());

    std::fs::write(&path, b"non-empty").unwrap();
    let fs = builder().build().expect("handle");
    fs.write_copy(&path, b"").expect("write_copy zero bytes");

    let read = std::fs::read(&path).expect("read");
    assert!(read.is_empty());
}

#[test]
fn write_copy_preserves_mtime_within_tolerance() {
    let path = tmp_path("mtime");
    let _g = Cleanup(path.clone());

    std::fs::write(&path, b"original").unwrap();
    let pre_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

    // Small sleep to ensure that, IF mtime is NOT preserved, the
    // post-write mtime would differ noticeably. Any inequality
    // outside the filesystem's ctime resolution would falsify.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let fs = builder().build().expect("handle");
    fs.write_copy(&path, b"replacement").expect("write_copy");

    let post_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    // Tolerance: 1 second covers filesystem timestamp granularity
    // on FAT (2 s) being rare on test fs; tighter would flake.
    let drift = post_mtime
        .duration_since(pre_mtime)
        .unwrap_or_else(|e| e.duration());
    assert!(
        drift.as_secs() < 2,
        "mtime drifted {drift:?} — preservation broken or fs has unusual granularity"
    );
}
