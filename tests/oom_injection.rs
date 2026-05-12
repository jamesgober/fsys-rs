//! 0.9.7 audit H-7 — OOM-injection coverage for fallible-alloc paths.
//!
//! Replaces the global allocator with `OomInjectingAllocator`
//! (via the internal `oom_inject` cargo feature) so tests can
//! force allocation failures at known sites. The
//! [`OomThreshold`] thread-local guard sets the minimum
//! allocation size that triggers `null` return; tests bracket
//! the code under test with the guard and verify the API
//! surfaces `Error::Io(OutOfMemory)` cleanly instead of
//! panicking.
//!
//! ## Why a separate test binary
//!
//! `#[global_allocator]` is a crate-level attribute. To replace
//! the system allocator only for these tests, this binary is
//! built with `--features oom_inject` while the rest of the test
//! suite + production builds use the system default. CI runs
//! this test in its own matrix entry (see `.github/workflows/ci.yml`).

#![cfg(feature = "oom_inject")]

use fsys::builder;
use fsys::test_support::OomThreshold;
#[cfg(target_os = "linux")]
use fsys::{Error, JournalOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_oom_inject_{}_{}_{tag}",
        std::process::id(),
        n
    ))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The simplest possible smoke test: with the threshold set to
/// `usize::MAX` (default), the allocator behaves exactly like
/// the system allocator. fsys operations succeed normally.
/// Confirms the wrapper allocator's pass-through path is sound.
#[test]
fn allocator_passes_through_when_threshold_is_unlimited() {
    // Default threshold = usize::MAX = never inject.
    let path = tmp_path("smoke");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    h.write(&path, b"smoke").expect("write");
    let data = h.read(&path).expect("read");
    assert_eq!(data, b"smoke");
}

/// OOM-injection at the threshold catches **large** allocations
/// (64 KiB AlignedBuf in this case). The threshold is set to
/// 2 KiB inside the guard, so any alloc of >= 2 KiB returns null.
/// `LogBuffer::new` triggers the threshold and is expected to
/// surface `Error::Io(OutOfMemory)`.
///
/// We exercise `AlignedBuf::new` indirectly via Direct-mode
/// journal open, which calls `AlignedBuf::new` to allocate the
/// dual log-buffer slots.
///
/// **Linux-only**: Windows / macOS may silently downgrade
/// `direct(true)` to buffered mode when the temp filesystem
/// rejects the elite open flags (e.g. tmpfs, FUSE, network
/// shares; `FILE_FLAG_NO_BUFFERING` on certain Windows
/// configurations). On a downgrade `LogBuffer` is never
/// allocated, so the OOM injection has nothing to trip. The
/// audit-level guarantee — clean `Error::Io(OutOfMemory)` from
/// `AlignedBuf::new` — is the same on every platform, but only
/// Linux gives us a deterministic way to force the path.
#[cfg(target_os = "linux")]
#[test]
fn direct_mode_journal_open_handles_oom_gracefully() {
    let path = tmp_path("direct_oom");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    // Pre-create the file so the journal-open path proceeds past
    // file-creation without needing big allocations of its own.
    std::fs::write(&path, b"").expect("create file");

    let result = {
        // Threshold = 2 KiB: any allocation >= 2 KiB fails.
        // LogBuffer::new tries to allocate two AlignedBufs of
        // capacity ≥ sector_size (typically 4 KiB); either of
        // those will trip the threshold and surface OOM.
        let _guard = OomThreshold::set(2048);
        h.journal_with(&path, JournalOptions::new().direct(true))
    };

    // Result must be Err — Direct-mode journal open requires
    // LogBuffer allocation, which fails under the threshold.
    // `JournalHandle` doesn't implement `Debug`, so we destructure
    // the `Result` manually rather than via `expect_err`.
    match result {
        Ok(_) => panic!("direct journal open under OOM must error, got Ok"),
        Err(Error::Io(io_err)) => {
            assert_eq!(
                io_err.kind(),
                std::io::ErrorKind::OutOfMemory,
                "expected ErrorKind::OutOfMemory, got {:?}",
                io_err.kind()
            );
        }
        Err(other) => panic!("expected Error::Io(OutOfMemory), got {:?}", other),
    }

    // After the guard drops, subsequent allocations work again.
    // Validate: the same operation succeeds with no threshold.
    let log = h
        .journal_with(&path, JournalOptions::new().direct(true))
        .expect("direct journal open succeeds without threshold");
    drop(log);
}

/// The threshold guard restores its prior value on Drop, even
/// if the test panics. This test deliberately constructs +
/// drops a guard via `catch_unwind` (no panic actually thrown),
/// then verifies the threshold is restored to the prior value.
#[test]
fn threshold_guard_restores_on_drop() {
    use fsys::test_support::OOM_THRESHOLD;

    // Capture the baseline (should be usize::MAX = no injection).
    let baseline = OOM_THRESHOLD.with(|c| c.get());
    assert_eq!(baseline, usize::MAX);

    {
        let _guard = OomThreshold::set(1024);
        let during = OOM_THRESHOLD.with(|c| c.get());
        assert_eq!(during, 1024, "threshold set inside guard scope");
    }

    let after = OOM_THRESHOLD.with(|c| c.get());
    assert_eq!(after, baseline, "threshold restored after guard drop");
}

/// Buffered-mode journal open does NOT need a large AlignedBuf
/// allocation (no log buffer). Under the same threshold that
/// blocks Direct-mode, buffered-mode opens cleanly. Validates
/// that the injection is targeted (size-based) and doesn't
/// accidentally break unrelated paths.
#[test]
fn buffered_mode_journal_open_unaffected_by_targeted_threshold() {
    let path = tmp_path("buffered_oom");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    std::fs::write(&path, b"").expect("create file");

    // Threshold = 2 KiB. Buffered-mode journal doesn't allocate
    // a log buffer — only path strings, small Vecs for the
    // group-commit state, etc., all well below 2 KiB.
    let _guard = OomThreshold::set(2048);
    let log = h
        .journal(&path)
        .expect("buffered journal open under threshold must succeed");
    drop(log);
}
