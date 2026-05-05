//! Integration tests for the 0.6.0 async layer (single-op CRUD).
//!
//! Validates that every `Handle::*_async` method works under a real
//! tokio runtime and routes through `spawn_blocking` per locked
//! decision D-1. Batch async is exercised separately in
//! `tests/async_batch.rs` (checkpoint F).

#![cfg(feature = "async")]

use fsys::builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_async_crud_{}_{}_{}",
        std::process::id(),
        n,
        tag
    ))
}

fn tmp_dir(tag: &str) -> PathBuf {
    let p = tmp_path(tag);
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn write_async_round_trips_via_read_async() {
    let path = tmp_path("rt");
    let _g = Cleanup(path.clone());

    let fs = Arc::new(builder().build().expect("handle"));

    fs.clone()
        .write_async(&path, b"hello async".to_vec())
        .await
        .expect("write_async");

    let read = fs.clone().read_async(&path).await.expect("read_async");
    assert_eq!(read, b"hello async");
}

#[tokio::test]
async fn write_at_async_overlays_at_offset() {
    let path = tmp_path("write_at");
    let _g = Cleanup(path.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_async(&path, b"00000000".to_vec())
        .await
        .expect("write");
    fs.clone()
        .write_at_async(&path, 2, b"XXX".to_vec())
        .await
        .expect("write_at");

    let read = fs.clone().read_async(&path).await.expect("read");
    assert_eq!(read, b"00XXX000");
}

#[tokio::test]
async fn read_at_async_returns_subslice() {
    let path = tmp_path("range");
    let _g = Cleanup(path.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_async(&path, b"abcdefghij".to_vec())
        .await
        .expect("write");

    let slice = fs
        .clone()
        .read_at_async(&path, 3, 4)
        .await
        .expect("read_at");
    assert_eq!(slice, b"defg");
}

#[tokio::test]
async fn delete_async_idempotent() {
    let path = tmp_path("del");
    let fs = Arc::new(builder().build().expect("handle"));

    fs.clone()
        .write_async(&path, b"x".to_vec())
        .await
        .expect("write");
    fs.clone().delete_async(&path).await.expect("delete");
    // Idempotent: a second delete of the same (now-missing) path
    // returns Ok.
    fs.clone()
        .delete_async(&path)
        .await
        .expect("second delete idempotent");
    let exists = fs.clone().exists_async(&path).await.expect("exists");
    assert!(!exists);
}

#[tokio::test]
async fn append_async_extends_file() {
    let path = tmp_path("append");
    let _g = Cleanup(path.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_async(&path, b"hello ".to_vec())
        .await
        .expect("write");
    fs.clone()
        .append_async(&path, b"world".to_vec())
        .await
        .expect("append");

    let read = fs.clone().read_async(&path).await.expect("read");
    assert_eq!(read, b"hello world");
}

#[tokio::test]
async fn write_copy_async_preserves_payload_round_trip() {
    let path = tmp_path("wcopy");
    let _g = Cleanup(path.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_async(&path, b"original".to_vec())
        .await
        .expect("write");
    fs.clone()
        .write_copy_async(&path, b"replaced".to_vec())
        .await
        .expect("write_copy");

    let read = fs.clone().read_async(&path).await.expect("read");
    assert_eq!(read, b"replaced");
}

#[tokio::test]
async fn truncate_async_resizes_file() {
    let path = tmp_path("trunc");
    let _g = Cleanup(path.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_async(&path, b"abcdefgh".to_vec())
        .await
        .expect("write");
    fs.clone().truncate_async(&path, 3).await.expect("truncate");

    let read = fs.clone().read_async(&path).await.expect("read");
    assert_eq!(read, b"abc");
}

#[tokio::test]
async fn rename_async_moves_file() {
    let src = tmp_path("rename_src");
    let dst = tmp_path("rename_dst");
    let _gs = Cleanup(src.clone());
    let _gd = Cleanup(dst.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_async(&src, b"payload".to_vec())
        .await
        .expect("write");
    fs.clone().rename_async(&src, &dst).await.expect("rename");

    assert!(!fs.clone().exists_async(&src).await.expect("exists src"));
    assert!(fs.clone().exists_async(&dst).await.expect("exists dst"));
}

#[tokio::test]
async fn copy_async_duplicates_file() {
    let src = tmp_path("copy_src");
    let dst = tmp_path("copy_dst");
    let _gs = Cleanup(src.clone());
    let _gd = Cleanup(dst.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_async(&src, b"payload".to_vec())
        .await
        .expect("write");
    let n = fs.clone().copy_async(&src, &dst).await.expect("copy");
    assert_eq!(n, 7);

    let read = fs.clone().read_async(&dst).await.expect("read");
    assert_eq!(read, b"payload");
}

#[tokio::test]
async fn meta_and_size_async_agree() {
    let path = tmp_path("meta");
    let _g = Cleanup(path.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    fs.clone()
        .write_async(&path, b"123456".to_vec())
        .await
        .expect("write");

    let size = fs.clone().size_async(&path).await.expect("size");
    let meta = fs.clone().meta_async(&path).await.expect("meta");
    assert_eq!(size, 6);
    assert_eq!(meta.size, 6);
}

#[tokio::test]
async fn mkdir_and_list_async() {
    let dir = tmp_dir("listdir");
    let _g = Cleanup(dir.clone());

    let fs = Arc::new(builder().build().expect("handle"));
    let nested = dir.join("a/b");
    fs.clone()
        .mkdir_all_async(&nested)
        .await
        .expect("mkdir_all");
    assert!(fs.clone().is_dir_async(&nested).await.expect("is_dir"));

    fs.clone()
        .write_async(&nested.join("file.txt"), b"x".to_vec())
        .await
        .expect("write");

    let entries = fs.clone().list_async(&nested).await.expect("list");
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn scan_find_count_async() {
    let dir = tmp_dir("walk");
    let _g = Cleanup(dir.clone());

    std::fs::write(dir.join("a.log"), b"x").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/b.log"), b"x").unwrap();
    std::fs::write(dir.join("sub/c.txt"), b"x").unwrap();

    let fs = Arc::new(builder().build().expect("handle"));

    let scanned = fs.clone().scan_all_async(&dir).await.expect("scan_all");
    assert_eq!(scanned.iter().filter(|e| e.is_file).count(), 3);

    let logs = fs.clone().find_async(&dir, "**/*.log").await.expect("find");
    assert_eq!(logs.len(), 2);

    let count = fs.clone().count_all_async(&dir).await.expect("count_all");
    assert_eq!(count, 3);
}

#[test]
fn async_method_outside_runtime_returns_error() {
    // Construct a handle synchronously, then call an async method
    // outside any tokio runtime. The method should return
    // `AsyncRuntimeRequired` rather than panicking.
    let fs = Arc::new(builder().build().expect("handle"));
    let path = tmp_path("no_runtime");

    // Drive the future to completion using a dummy poll. We can't
    // easily await from a sync test, but the runtime check happens
    // at the start of the future before any await — so polling
    // once should surface the error.
    let fut = fs.clone().write_async(&path, b"x".to_vec());
    futures_check(fut);
}

/// Tiny synchronous future poller for the runtime-required test.
/// Pins the future on the stack and polls it once with a no-op
/// waker; if the future returns `Ready(Err(AsyncRuntimeRequired))`
/// (the expected state when no runtime is active) the test passes.
fn futures_check(fut: impl std::future::Future<Output = fsys::Result<()>>) {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn noop_waker() -> Waker {
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        let raw = RawWaker::new(std::ptr::null(), &VTABLE);
        // SAFETY: the vtable's clone/wake/wake_by_ref/drop are all
        // no-ops; the vtable itself has 'static lifetime; the data
        // pointer is null and never dereferenced.
        unsafe { Waker::from_raw(raw) }
    }

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    // SAFETY: `fut` lives on this stack frame for the duration of
    // the poll; `Pin::new_unchecked` is sound because we never move
    // the future after pinning.
    let mut pinned = Box::pin(fut);
    match Pin::new(&mut pinned).poll(&mut cx) {
        Poll::Ready(Err(fsys::Error::AsyncRuntimeRequired)) => {} // expected
        Poll::Ready(other) => panic!("unexpected ready outcome: {other:?}"),
        Poll::Pending => panic!("expected immediate Ready(Err(AsyncRuntimeRequired))"),
    }
}
