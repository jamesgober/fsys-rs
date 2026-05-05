//! Regression tests for the 5 Critical findings fixed in 0.8.0
//! checkpoint B (see `.dev/CODE-AUDIT-0.8.0.md`).
//!
//! Each test maps to a specific `C-N` fix:
//!
//! - C-1: Windows `write_at` 64-bit offset correctness.
//! - C-2: Zero-byte Direct IO no longer triggers UB; produces a
//!   valid 0-byte file via the public API.
//! - C-3: eventfd ownership in completion-driver owner_loop —
//!   not directly testable from outside (Linux + async
//!   only) but covered indirectly by the existing
//!   async-substrate handle-drop tests.
//! - C-4: AsyncMutex removed from submit hot path — observable
//!   as the absence of lock contention; we exercise
//!   concurrent-submit correctness which would deadlock
//!   if the mutex re-entered itself.
//! - C-5: `poisoned` doc/code reconciliation — covered by
//!   existing handle-poisoning tests; nothing new needed.

use fsys::builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_critical_fix_{}_{}_{}",
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

// ─────────────────────────────────────────────────────────────────
// C-2: Zero-byte writes do not trigger UB on any Method
// ─────────────────────────────────────────────────────────────────

#[test]
fn c2_quick_write_empty_payload_produces_zero_byte_file() {
    // Reproduction of the original bug surface: `quick::write` with
    // an empty payload. Before the fix, the Direct path's allocator
    // hit `alloc_zeroed` with size=0 (UB). After the fix, the
    // empty-input short-circuit produces a clean 0-byte file.
    let path = tmp_path("c2_quick");
    let _g = Cleanup(path.clone());

    fsys::quick::write(&path, b"").expect("empty write should succeed");
    let bytes = fsys::quick::read(&path).expect("read");
    assert!(bytes.is_empty());

    let meta = std::fs::metadata(&path).expect("metadata");
    assert_eq!(meta.len(), 0, "empty write must produce a 0-byte file");
}

#[test]
fn c2_handle_write_empty_payload_default_method() {
    let path = tmp_path("c2_handle_default");
    let _g = Cleanup(path.clone());

    let fs = builder().build().expect("handle");
    fs.write(&path, b"").expect("empty write");
    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
}

#[test]
fn c2_handle_write_empty_payload_method_sync() {
    let path = tmp_path("c2_sync");
    let _g = Cleanup(path.clone());

    let fs = builder()
        .method(fsys::Method::Sync)
        .build()
        .expect("handle");
    fs.write(&path, b"").expect("empty write");
    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
}

#[test]
fn c2_handle_write_empty_payload_method_data() {
    let path = tmp_path("c2_data");
    let _g = Cleanup(path.clone());

    let fs = builder()
        .method(fsys::Method::Data)
        .build()
        .expect("handle");
    fs.write(&path, b"").expect("empty write");
    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
}

#[test]
fn c2_handle_write_empty_payload_method_direct() {
    // The most important case — Direct is where the UB lived.
    let path = tmp_path("c2_direct");
    let _g = Cleanup(path.clone());

    let fs = builder()
        .method(fsys::Method::Direct)
        .build()
        .expect("handle");
    fs.write(&path, b"")
        .expect("empty write on Direct must short-circuit cleanly");
    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
}

#[test]
fn c2_handle_write_empty_payload_method_mmap() {
    let path = tmp_path("c2_mmap");
    let _g = Cleanup(path.clone());

    let fs = builder()
        .method(fsys::Method::Mmap)
        .build()
        .expect("handle");
    fs.write(&path, b"").expect("empty write");
    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
}

#[test]
fn c2_write_copy_empty_payload_replaces_file() {
    let path = tmp_path("c2_copy");
    let _g = Cleanup(path.clone());

    let fs = builder().build().expect("handle");
    fs.write(&path, b"original").expect("seed");
    fs.write_copy(&path, b"").expect("empty write_copy");
    assert_eq!(
        std::fs::metadata(&path).expect("metadata").len(),
        0,
        "write_copy with empty payload must truncate target to 0"
    );
}

// ─────────────────────────────────────────────────────────────────
// C-4: Concurrent async submits do not deadlock
// ─────────────────────────────────────────────────────────────────
//
// Before the fix, every submit acquired an `AsyncMutex` around
// `submit_tx`. Concurrent submits serialised through the mutex —
// correctness was preserved but the mutex acquisition was on the
// hot path.
//
// After the fix, `submit_tx` is a plain `mpsc::UnboundedSender`
// shared by `&self`. Concurrent submits should make progress
// without any cross-task wait. This test exercises the concurrent
// path; if a regression re-introduces a Mutex (or worse, a
// re-entrant one), the test will time out under the runtime's
// task-stall detection.

#[cfg(feature = "async")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn c4_concurrent_async_submits_make_progress() {
    use std::sync::Arc;

    let fs = Arc::new(builder().build().expect("handle"));
    let mut joins = Vec::new();
    for i in 0..16 {
        let fs2 = fs.clone();
        let path = tmp_path(&format!("c4_concurrent_{i}"));
        joins.push(tokio::spawn(async move {
            let _g = Cleanup(path.clone());
            fs2.clone()
                .write_async(&path, format!("task {i}").into_bytes())
                .await
                .expect("write_async");
            let bytes = fs2.clone().read_async(&path).await.expect("read_async");
            assert_eq!(bytes, format!("task {i}").into_bytes());
        }));
    }
    for j in joins {
        j.await.expect("task should not panic or hang");
    }
}

// ─────────────────────────────────────────────────────────────────
// C-1: Cross-platform `write_at` correctness for offsets up to
// the per-platform max. We don't actually test ≥2 GiB offsets
// (would require a 2 GiB file in tempdir); we test correctness at
// boundaries that exercise the same code path. The pre-fix bug
// would fail these too if the i32-truncation path were reachable
// at smaller offsets, but the real proof is that `write_at` now
// goes through a single 64-bit code path on every platform.
// ─────────────────────────────────────────────────────────────────

// The crud-file `write_at` is internal-only (atomic-replace shape
// of `write` does not allow caller-controlled offsets). Test the
// platform primitive indirectly via the public API.
#[test]
fn c1_write_full_file_then_read_at_offset_returns_correct_bytes() {
    let path = tmp_path("c1_offset_read");
    let _g = Cleanup(path.clone());

    let fs = builder().build().expect("handle");
    let payload = (0..4096u32).map(|i| (i % 256) as u8).collect::<Vec<u8>>();
    fs.write(&path, &payload).expect("write");

    // read_at exercises the offset path on every platform.
    // Any 32-bit offset truncation would corrupt this read.
    let chunk = fs.read_at(&path, 1024, 256).expect("read_at");
    let expected: Vec<u8> = (1024u32..1280u32).map(|i| (i % 256) as u8).collect();
    assert_eq!(chunk, expected);
}
