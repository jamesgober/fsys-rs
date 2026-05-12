//! 0.9.7 audit H-9 — kernel-version fallback path tests.
//!
//! Exercises the **no-elite-flags** baseline that ships when the
//! running kernel rejects `COOP_TASKRUN` / `SINGLE_ISSUER` /
//! `DEFER_TASKRUN`, plus the `fallocate` → `posix_fallocate`
//! fallback for old kernels / filesystems without `fallocate`.
//!
//! These paths existed since 0.9.4 but had no explicit test
//! coverage — CI just ran whatever the runner's kernel
//! happened to provide. The audit (H-9) flagged this as a gap.
//!
//! ## Mechanism
//!
//! Two env-var test hooks added in 0.9.7:
//!
//! - `FSYS_TEST_FORCE_NO_IOURING_FEATURES=1` — `iouring_features::probe`
//!   returns the all-false default without touching the kernel.
//!   Forces the pre-0.9.4 baseline path on any kernel.
//! - `FSYS_TEST_FORCE_POSIX_FALLOCATE=1` — `platform::linux::preallocate`
//!   skips the `fallocate(2)` attempt and goes straight to
//!   `posix_fallocate(3)`. Forces the fallback path on any
//!   filesystem.
//!
//! ## Process isolation
//!
//! Env vars are process-global. Cargo runs each integration
//! test binary in its own process, so the env vars set here
//! don't bleed into other tests. The `OnceLock` cache inside
//! `iouring_features` is per-process — the first test in this
//! binary to call `features()` populates the cache with the
//! forced value, and all subsequent tests in this binary see
//! the same forced value.

#![cfg(target_os = "linux")]

use fsys::{builder, JournalReader};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_iouring_fallback_{}_{}_{tag}",
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

/// Set both fallback-forcing env vars before any fsys code runs.
/// Idempotent — `std::sync::Once` ensures we only set each var
/// once even if multiple tests call this. The `OnceLock` cache
/// inside `iouring_features` will pick up the forced state on
/// the first probe.
///
/// `env::set_var` is `safe fn` in edition 2021 (which this crate
/// uses). Edition 2024 marked it `unsafe fn` due to thread-safety
/// concerns around concurrent reads, but in this test-binary
/// startup context (before any worker threads exist) the
/// invariant is satisfied trivially.
fn force_fallback_paths() {
    use std::sync::Once;
    static SET: Once = Once::new();
    SET.call_once(|| {
        std::env::set_var("FSYS_TEST_FORCE_NO_IOURING_FEATURES", "1");
        std::env::set_var("FSYS_TEST_FORCE_POSIX_FALLOCATE", "1");
    });
}

#[test]
fn handle_construction_succeeds_with_no_iouring_features() {
    force_fallback_paths();
    // Handle::new probes hardware (which constructs an io_uring
    // ring internally to test capability). With the env var set,
    // the elite-flag probe returns all-false; the ring construction
    // path uses no setup flags — exactly the pre-0.9.4 baseline.
    // Construction must succeed cleanly.
    let _h = builder()
        .build()
        .expect("handle should construct without elite flags");
}

#[test]
fn journal_write_read_round_trip_no_iouring_features() {
    force_fallback_paths();
    let path = tmp_path("no_iouring_features");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    let log = h.journal(&path).expect("journal");

    // Append several records — exercises the baseline io_uring
    // path (without DEFER_TASKRUN, SINGLE_ISSUER, COOP_TASKRUN)
    // OR the spawn_blocking fallback if io_uring isn't
    // available at all on the runner.
    let lsn1 = log.append(b"alpha").expect("append alpha");
    let lsn2 = log.append(b"beta").expect("append beta");
    let lsn3 = log.append(b"gamma").expect("append gamma");
    assert!(lsn3 > lsn2 && lsn2 > lsn1);

    // Group-commit sync — exercises the fsync path through the
    // sync ring (which uses RingMode::Sync; elite flags are
    // also gated by features(), so this exercises that path
    // with no flags applied).
    log.sync_through(lsn3).expect("sync_through");
    assert!(log.synced_lsn() >= lsn3);

    log.close().expect("close");

    // Read-back via JournalReader — verifies framing + CRC are
    // byte-for-byte correct under the no-elite-flag path.
    let mut reader = JournalReader::open(&path).expect("reader");
    let payloads: Vec<Vec<u8>> = reader.iter().map(|r| r.expect("decode").payload).collect();
    assert_eq!(
        payloads,
        vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]
    );
    assert_eq!(reader.tail_state(), fsys::JournalTailState::CleanEnd);
}

#[test]
fn write_round_trip_no_iouring_features() {
    // Handle::write (atomic-replace) — exercises the
    // crud/file.rs path which may use the sync io_uring ring
    // via Method::Direct or fall back to pwrite. With elite
    // flags off, the ring is constructed without them — same
    // pre-0.9.4 behaviour.
    force_fallback_paths();
    let path = tmp_path("write_no_iouring");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    let data = b"baseline-no-elite-flags";
    h.write(&path, data).expect("write");
    let read_back = h.read(&path).expect("read");
    assert_eq!(read_back, data);
}

#[test]
fn preallocate_falls_back_to_posix_fallocate_cleanly() {
    // FSYS_TEST_FORCE_POSIX_FALLOCATE=1 makes `preallocate`
    // skip the `fallocate(2)` attempt and go straight to
    // `posix_fallocate(3)`. This test calls the public
    // `JournalHandle::preallocate` API explicitly so the
    // fallback path is actually invoked — and verifies:
    //   1. No panic, no hang
    //   2. Returns `Ok(())`
    //   3. File is at least `len` bytes after the call
    //      (`posix_fallocate` extends the file as it writes
    //      zero-bytes; that's its semantic vs `fallocate`
    //      which can keep the size with `FALLOC_FL_KEEP_SIZE`)
    force_fallback_paths();
    let path = tmp_path("preallocate_fallback");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    let log = h.journal(&path).expect("journal");

    const RESERVE_BYTES: u64 = 64 * 1024;
    log.preallocate(0, RESERVE_BYTES)
        .expect("preallocate must take the posix_fallocate fallback cleanly");

    // posix_fallocate writes zeros and extends the file; the
    // file size must be at least RESERVE_BYTES after the call.
    log.close().expect("close");
    let meta = std::fs::metadata(&path).expect("stat");
    assert!(
        meta.len() >= RESERVE_BYTES,
        "file size after posix_fallocate fallback: {} < expected {}",
        meta.len(),
        RESERVE_BYTES
    );
}
