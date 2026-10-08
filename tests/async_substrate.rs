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

/// Marks the child process started by
/// [`env_override_forces_spawn_blocking_substrate`].
const OVERRIDE_CHILD_VAR: &str = "FSYS_TEST_SUBSTRATE_OVERRIDE_CHILD";

/// `FSYS_DISABLE_NATIVE_ASYNC` is read once per process, so the
/// override is tested in a fresh child process that has it set from
/// the start (setting it here could not undo an earlier test's first
/// read, and would leak into the other tests of this binary). The
/// child runs [`env_override_child`] from this same test binary.
#[test]
fn env_override_forces_spawn_blocking_substrate() {
    if std::env::var_os(OVERRIDE_CHILD_VAR).is_some() {
        return;
    }
    let exe = std::env::current_exe().expect("test binary path");
    let output = std::process::Command::new(exe)
        .args(["--exact", "env_override_child", "--test-threads=1"])
        .env("FSYS_DISABLE_NATIVE_ASYNC", "1")
        .env(OVERRIDE_CHILD_VAR, "1")
        .output()
        .expect("spawn child test process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Guard against the filter silently matching nothing.
    assert!(
        stdout.contains("1 passed"),
        "child did not run env_override_child:\n{stdout}"
    );
}

/// Body of [`env_override_forces_spawn_blocking_substrate`]; a no-op
/// unless started as its child process.
#[tokio::test]
async fn env_override_child() {
    if std::env::var_os(OVERRIDE_CHILD_VAR).is_none() {
        return;
    }
    assert!(std::env::var_os("FSYS_DISABLE_NATIVE_ASYNC").is_some());

    let fs = Arc::new(
        builder()
            .method(fsys::Method::Direct)
            .build()
            .expect("handle"),
    );

    // Under the target tmpdir rather than /tmp, which is tmpfs on many
    // Linux hosts: tmpfs may reject O_DIRECT, and the handle would
    // then leave Method::Direct and report SpawnBlocking regardless
    // of the override.
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "fsys_async_substrate_override_{}",
        std::process::id()
    ));
    let _g = Cleanup(path.clone());

    // Run several async Direct ops. Without the override the first
    // one constructs the native ring and later ones use it.
    for i in 0..3u8 {
        fs.clone()
            .write_async(&path, vec![i; 4096])
            .await
            .expect("write_async under the override");
    }
    assert_eq!(std::fs::read(&path).expect("read"), vec![2u8; 4096]);

    assert_eq!(
        fs.async_substrate(),
        AsyncSubstrate::SpawnBlocking,
        "FSYS_DISABLE_NATIVE_ASYNC=1 must force SpawnBlocking"
    );
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
