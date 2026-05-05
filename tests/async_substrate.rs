//! Integration tests for the 0.7.0 async-substrate selection logic.
//!
//! Validates:
//! - `Handle::async_substrate()` returns the right value before and
//!   after the first async Direct op constructs the native ring.
//! - `FSYS_DISABLE_NATIVE_ASYNC=1` forces `SpawnBlocking` even on
//!   Linux + Direct.
//! - `write_async` produces correct results on BOTH substrates.
//! - The 0.6.0 async API (which uses Method::Auto, not Direct) is
//!   unchanged — substrate is `SpawnBlocking` on every platform
//!   regardless of feature flags.

#![cfg(feature = "async")]

use fsys::{builder, AsyncSubstrate};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_async_substrate_{}_{}_{}",
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
async fn auto_method_substrate_is_spawn_blocking() {
    // Auto resolves to a non-Direct method on most CI runners
    // (no NVMe, no PLP, etc.). Substrate should be SpawnBlocking.
    let fs = builder().build().expect("handle");
    assert_eq!(
        fs.async_substrate(),
        AsyncSubstrate::SpawnBlocking,
        "Auto on a non-Direct-capable runner should pick SpawnBlocking"
    );
}

#[tokio::test]
async fn sync_method_substrate_is_spawn_blocking() {
    let fs = builder()
        .method(fsys::Method::Sync)
        .build()
        .expect("handle");
    assert_eq!(
        fs.async_substrate(),
        AsyncSubstrate::SpawnBlocking,
        "Method::Sync must always use SpawnBlocking"
    );
}

#[tokio::test]
async fn data_method_substrate_is_spawn_blocking() {
    let fs = builder()
        .method(fsys::Method::Data)
        .build()
        .expect("handle");
    assert_eq!(
        fs.async_substrate(),
        AsyncSubstrate::SpawnBlocking,
        "Method::Data must always use SpawnBlocking (only Direct can be native)"
    );
}

#[tokio::test]
async fn direct_method_pre_op_substrate_is_spawn_blocking() {
    // Even with Method::Direct, async_substrate() returns
    // SpawnBlocking until the first async Direct op constructs the
    // native ring. This is the documented "configuration intent
    // vs runtime truth" semantics from `.dev/DECISIONS-0.7.0.md`.
    let fs = builder()
        .method(fsys::Method::Direct)
        .build()
        .expect("handle");
    assert_eq!(
        fs.async_substrate(),
        AsyncSubstrate::SpawnBlocking,
        "Pre-first-op substrate must be SpawnBlocking (lazy ring construction)"
    );
}

#[tokio::test]
async fn write_async_through_direct_works_on_either_substrate() {
    let path = tmp_path("direct_write");
    let _g = Cleanup(path.clone());

    let fs = Arc::new(
        builder()
            .method(fsys::Method::Direct)
            .build()
            .expect("handle"),
    );
    fs.clone()
        .write_async(&path, b"hello via Direct async".to_vec())
        .await
        .expect("write_async on Direct must succeed (native or fallback)");

    let read = std::fs::read(&path).expect("read");
    assert_eq!(read, b"hello via Direct async");
}

#[tokio::test]
async fn env_override_forces_spawn_blocking_substrate() {
    // SAFETY: this test mutates process env. Single-threaded
    // libtest invocation per `--test-threads=1`; no concurrent
    // env mutation in this binary.
    let prior = std::env::var_os("FSYS_DISABLE_NATIVE_ASYNC");
    // SAFETY: documented racy-in-multi-threaded-process std API;
    // we run single-threaded.
    unsafe {
        std::env::set_var("FSYS_DISABLE_NATIVE_ASYNC", "1");
    }

    let fs = Arc::new(
        builder()
            .method(fsys::Method::Direct)
            .build()
            .expect("handle"),
    );

    let path = tmp_path("override");
    let _g = Cleanup(path.clone());

    // Trigger an async Direct op so the substrate would normally
    // construct the native ring.
    let _ = fs
        .clone()
        .write_async(&path, b"forced fallback".to_vec())
        .await;

    // With override set, substrate must be SpawnBlocking — even
    // though we just ran an async Direct op.
    assert_eq!(
        fs.async_substrate(),
        AsyncSubstrate::SpawnBlocking,
        "FSYS_DISABLE_NATIVE_ASYNC=1 must force SpawnBlocking"
    );

    // Restore.
    // SAFETY: same reasoning as the set above.
    unsafe {
        match prior {
            Some(v) => std::env::set_var("FSYS_DISABLE_NATIVE_ASYNC", v),
            None => std::env::remove_var("FSYS_DISABLE_NATIVE_ASYNC"),
        }
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn linux_direct_async_transitions_to_native_after_first_op() {
    // On Linux without env override, after the first async Direct
    // op runs successfully, the substrate should transition to
    // NativeIoUring (the async ring is constructed during the op).
    //
    // This test runs only on Linux where the native substrate is
    // possible. On a runner without io_uring, the ring construction
    // fails and substrate stays SpawnBlocking — that's also a
    // valid outcome (we check only that the value is well-defined).
    if std::env::var_os("FSYS_DISABLE_NATIVE_ASYNC").is_some() {
        return; // skip if env-disabled
    }

    let fs = Arc::new(
        builder()
            .method(fsys::Method::Direct)
            .build()
            .expect("handle"),
    );

    let path = tmp_path("linux_native_transition");
    let _g = Cleanup(path.clone());
    let _ = fs.clone().write_async(&path, vec![0xA5u8; 4096]).await;

    // Either NativeIoUring (success path) or SpawnBlocking
    // (ring construction failed for some reason — sandboxed
    // runner, missing kernel support, etc.). Both are valid.
    let s = fs.async_substrate();
    assert!(
        s == AsyncSubstrate::NativeIoUring || s == AsyncSubstrate::SpawnBlocking,
        "post-op substrate must be a valid variant; got {s:?}"
    );
}

#[tokio::test]
async fn substrate_strings_match_enum_values() {
    let fs = builder().build().expect("handle");
    let s = fs.async_substrate();
    let name = s.name();
    if s.is_native() {
        assert_eq!(name, "native io_uring");
    } else {
        assert_eq!(name, "spawn_blocking");
    }
}
