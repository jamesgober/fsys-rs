//! 0.9.7 audit M-5 — cross-platform symmetry tests for critical
//! behaviours.
//!
//! The 0.9.6 audit flagged that several "Linux-only stress tests"
//! left the Windows + macOS code paths for the same behaviours
//! untested in CI. The bulk of that gap was already closed
//! (`crash_journal.rs` runs on all three OSes; `nvme_passthrough.rs`
//! has cross-platform variants). This file fills the *explicit*
//! gap: a suite of tests that run on every platform and assert the
//! same critical contract on each, with platform-specific
//! assertions where the underlying primitive legitimately differs.
//!
//! ## Why a dedicated file
//!
//! Distributing these assertions across the existing test files
//! mixes concerns: a test that's "cross-platform" sits next to
//! tests that target a specific code path. This file makes the
//! contract — "the same observable behaviour on every supported
//! platform" — the **subject** of the tests rather than an
//! implicit property.
//!
//! Each test below executes on Linux, macOS, and Windows. The
//! `#[cfg(target_os = ...)]` branches inside the tests target the
//! **expected primitive name** (`fdatasync` vs `FlushFileBuffers`
//! vs `F_FULLFSYNC`), not the test's *outcome*. The outcome
//! (durable write, recoverable journal, no panic on edge inputs)
//! is the same on every platform — that's the symmetry the
//! 0.9.7 audit M-5 was asking us to make explicit.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use fsys::{builder, JournalOptions, JournalReader, JournalTailState, Method};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_plat_sym_{}_{}_{tag}",
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

/// Handle::write / Handle::read round-trip succeeds on every
/// platform with byte-for-byte fidelity. Establishes the baseline
/// that the Method::Sync (default) code path produces identical
/// observable behaviour across Linux / macOS / Windows.
#[test]
fn write_read_round_trip_is_platform_symmetric() {
    let path = tmp_path("write_read");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");

    let payload = b"\x00\x01\x02\xff\xfe\xfd\xaa\x55\xa5\x5a";
    h.write(&path, payload).expect("write");
    let read_back = h.read(&path).expect("read");
    assert_eq!(read_back, payload, "round-trip must be byte-for-byte");
}

/// Direct-mode write succeeds on every platform — either via the
/// platform's elite Direct-IO primitive (`O_DIRECT` /
/// `FILE_FLAG_NO_BUFFERING` / `F_NOCACHE`) or via the silent
/// fallback when the temp filesystem rejects it. The audit-level
/// contract is "Direct must never panic / hang; either honour the
/// hint or fall back cleanly" — verified here on every OS.
#[test]
fn direct_write_round_trip_is_platform_symmetric() {
    let path = tmp_path("direct_write");
    let _g = Cleanup(path.clone());
    let h = builder().method(Method::Direct).build().expect("handle");

    let payload = b"direct-mode write must round-trip on every supported platform";
    h.write(&path, payload).expect("direct write");
    let read_back = h.read(&path).expect("read");
    assert_eq!(read_back, payload);
}

/// Buffered-mode journal append + sync + read-back round-trips on
/// every platform. The journal substrate is the load-bearing
/// HiveDB primitive — its observable behaviour MUST be identical
/// across OS.
#[test]
fn journal_buffered_round_trip_is_platform_symmetric() {
    let path = tmp_path("journal_buffered");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    let log = h.journal(&path).expect("journal");

    let lsn_a = log.append(b"alpha").expect("append alpha");
    let lsn_b = log.append(b"beta").expect("append beta");
    let lsn_c = log.append(b"gamma").expect("append gamma");
    assert!(lsn_c > lsn_b && lsn_b > lsn_a);

    log.sync_through(lsn_c).expect("sync_through");
    assert!(log.synced_lsn() >= lsn_c);

    log.close().expect("close");

    // Read-back via JournalReader.
    let mut reader = JournalReader::open(&path).expect("reader");
    let payloads: Vec<Vec<u8>> = reader.iter().map(|r| r.expect("decode").payload).collect();
    assert_eq!(
        payloads,
        vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]
    );
    assert_eq!(reader.tail_state(), JournalTailState::CleanEnd);
}

/// Direct-mode journal append + sync + read-back round-trips on
/// every platform. Direct mode may downgrade to buffered on
/// filesystems that reject the elite flag (tmpfs / FUSE on Linux,
/// network shares on Windows). The contract — "open with
/// `direct(true)`, append, sync, read back identical bytes" —
/// holds either way.
#[test]
fn journal_direct_round_trip_is_platform_symmetric() {
    let path = tmp_path("journal_direct");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    let log = h
        .journal_with(&path, JournalOptions::new().direct(true))
        .expect("direct journal");

    let lsn = log.append(b"direct-record").expect("append");
    log.sync_through(lsn).expect("sync");
    log.close().expect("close");

    let mut reader = JournalReader::open(&path).expect("reader");
    let payloads: Vec<Vec<u8>> = reader.iter().map(|r| r.expect("decode").payload).collect();
    assert_eq!(payloads, vec![b"direct-record".to_vec()]);
}

/// Empty-payload journal record (12-byte header-only frame)
/// round-trips on every platform — this is the framing-format
/// boundary case from 0.9.7 M-11. Validates that framing/CRC
/// behaviour is byte-for-byte identical across OSes (the audit
/// asked specifically: is there any platform where empty records
/// behave differently? The answer must remain "no").
#[test]
fn empty_record_round_trip_is_platform_symmetric() {
    let path = tmp_path("empty_record");
    let _g = Cleanup(path.clone());
    let h = builder().build().expect("handle");
    let log = h.journal(&path).expect("journal");

    let lsn = log.append(b"").expect("append empty");
    log.sync_through(lsn).expect("sync");
    log.close().expect("close");

    let mut reader = JournalReader::open(&path).expect("reader");
    let payloads: Vec<Vec<u8>> = reader.iter().map(|r| r.expect("decode").payload).collect();
    assert_eq!(payloads, vec![Vec::<u8>::new()]);
    assert_eq!(reader.tail_state(), JournalTailState::CleanEnd);

    let meta = std::fs::metadata(&path).expect("stat");
    assert_eq!(
        meta.len(),
        12,
        "empty record must produce exactly 12 bytes \
         (magic + len + crc32c) on every platform — got {} bytes",
        meta.len()
    );
}

/// `active_durability_primitive` reports the platform's expected
/// canonical primitive for each `Method` — this is the explicit
/// platform-symmetry contract on the durability primitive. The
/// per-platform asserts catch regressions where a code path
/// downgraded to a slower primitive without the public accessor
/// reflecting the change.
#[test]
fn active_durability_primitive_per_method_is_documented() {
    use fsys::primitive;

    let sync = builder()
        .method(Method::Sync)
        .build()
        .expect("handle Sync");
    let data = builder()
        .method(Method::Data)
        .build()
        .expect("handle Data");

    let p_sync = sync.active_durability_primitive();
    let p_data = data.active_durability_primitive();

    #[cfg(target_os = "linux")]
    {
        assert_eq!(p_sync, primitive::FSYNC);
        assert_eq!(p_data, primitive::FDATASYNC);
    }
    #[cfg(target_os = "macos")]
    {
        // macOS: Sync uses F_FULLFSYNC (the only true media
        // durability primitive); Data degenerates to the same
        // since `sync_data` has no separate primitive on Darwin.
        assert_eq!(p_sync, primitive::F_FULLFSYNC);
        assert_eq!(p_data, primitive::F_FULLFSYNC);
    }
    #[cfg(target_os = "windows")]
    {
        // Windows: FlushFileBuffers is the only fsync-equivalent;
        // sync_data also delegates to it (no separate
        // FlushDataOnly primitive).
        assert_eq!(p_sync, primitive::FSYNC);
        assert_eq!(p_data, primitive::FSYNC);
    }
}

/// Reads against a non-existent path return a clean
/// `Error::Io(NotFound)` on every platform — no panic, no
/// platform-specific error variant leak.
#[test]
fn read_missing_path_returns_clean_error_on_every_platform() {
    let path = tmp_path("definitely_not_there");
    // Don't create the file — read it cold.
    let h = builder().build().expect("handle");

    match h.read(&path) {
        Ok(_) => panic!("read of non-existent path must fail"),
        Err(fsys::Error::Io(io_err)) => {
            assert_eq!(
                io_err.kind(),
                std::io::ErrorKind::NotFound,
                "expected NotFound, got {:?}",
                io_err.kind()
            );
        }
        Err(other) => panic!(
            "expected Error::Io(NotFound), got {:?} \
             (cross-platform contract requires NotFound)",
            other
        ),
    }
}
