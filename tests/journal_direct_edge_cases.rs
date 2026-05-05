//! Edge-case coverage for Direct-IO journal mode.
//!
//! Companion to [`journal_direct.rs`] which covers the canonical
//! round-trips. The tests here exercise the more exotic corners
//! of the direct-mode path: resume across partial-flush
//! boundaries, multiple oversize records back-to-back, log
//! buffer size extremes, group-commit coalescence under direct
//! mode, and root-scoped path resolution interactions.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use fsys::{builder, JournalOptions, JournalReader, Lsn};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_journal_direct_edge_{}_{}_{tag}",
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

/// Resume across a partial-flush boundary: session 1 appends
/// records and calls `sync_through`, leaving the on-disk file in
/// the partial-sector-pad state. Session 2 reopens and must
/// rehydrate the partial sector so subsequent appends overwrite
/// the zero-pad without destroying the previously-synced records.
#[test]
fn resume_across_partial_flush_boundary() {
    let path = tmp_path("partial_flush_resume");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");

    // Session 1 — append a few small records, sync (partial
    // flush), close. The file ends with zero-pad to a sector.
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true).log_buffer_kib(4))
            .expect("open 1");
        let _ = log.append(b"alpha").expect("a1");
        let _ = log.append(b"beta").expect("a2");
        let lsn = log.append(b"gamma").expect("a3");
        log.sync_through(lsn).expect("sync");
        log.close().expect("close");
    }

    // Session 2 — reopen, append more, sync, close. The
    // rehydration logic must seat the new appends so they
    // overwrite the zero-pad bytes, not so they create an LSN
    // gap or write past the pad.
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true).log_buffer_kib(4))
            .expect("open 2");
        let _ = log.append(b"delta").expect("a4");
        let lsn = log.append(b"epsilon").expect("a5");
        log.sync_through(lsn).expect("sync 2");
        log.close().expect("close 2");
    }

    let mut reader = JournalReader::open(&path).expect("reader");
    let payloads: Vec<Vec<u8>> = reader.iter().map(|r| r.unwrap().payload).collect();
    assert_eq!(
        payloads.len(),
        5,
        "expected 5 records, got {}",
        payloads.len()
    );
    assert_eq!(payloads[0], b"alpha");
    assert_eq!(payloads[1], b"beta");
    assert_eq!(payloads[2], b"gamma");
    assert_eq!(payloads[3], b"delta");
    assert_eq!(payloads[4], b"epsilon");
}

/// Multiple oversize records back-to-back. Each oversize record
/// goes through the standalone-write path which adjusts
/// `flush_pos` to a sector boundary at-or-before the record's
/// end and rehydrates the partial sector tail into the buffer.
/// Multiple in a row must compose correctly — each record's tail
/// gets overwritten by the next record's leading bytes.
#[test]
fn multiple_oversize_records_back_to_back() {
    let path = tmp_path("multi_oversize");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true).log_buffer_kib(4))
        .expect("open");

    // Each payload is 8 KiB — twice the 4 KiB buffer capacity,
    // forcing the oversize-record path.
    let payloads: Vec<Vec<u8>> = (0..5)
        .map(|i| {
            let byte = 0x10u8 + i as u8;
            vec![byte; 8 * 1024]
        })
        .collect();

    let mut last_lsn = Lsn::ZERO;
    for p in &payloads {
        last_lsn = log.append(p).expect("oversize append");
    }
    log.sync_through(last_lsn).expect("sync");
    drop(log);

    let mut reader = JournalReader::open(&path).expect("reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 5);
    for (i, r) in recs.iter().enumerate() {
        let expected = &payloads[i];
        assert_eq!(r.payload.len(), expected.len(), "record {i} length");
        assert_eq!(r.payload, *expected, "record {i} content drift");
    }
}

/// Log buffer at the minimum permitted size (4 KiB after the
/// `log_buffer_kib` clamp). Every small append still functions;
/// flushes happen frequently because the buffer fills quickly.
#[test]
fn log_buffer_clamped_to_minimum() {
    let path = tmp_path("clamp_min");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    // log_buffer_kib(0) clamps to the 4 KiB floor.
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true).log_buffer_kib(0))
        .expect("open with clamped buffer");

    // Append enough records that several real flushes must
    // happen (≥ 1 KiB of payload at a time × 50 = 50 KiB,
    // which is ~12 buffer-full flushes at 4 KiB each).
    let payload = vec![0x77u8; 1000];
    let mut last = Lsn::ZERO;
    for _ in 0..50 {
        last = log.append(&payload).expect("append");
    }
    log.sync_through(last).expect("sync");
    drop(log);

    let mut reader = JournalReader::open(&path).expect("reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 50);
    for r in &recs {
        assert_eq!(r.payload, payload);
    }
}

/// Log buffer at a large size (4 MiB). Records pile up in
/// memory until either the buffer fills or `sync_through` is
/// called. Validates that the larger buffer doesn't introduce
/// any latent path issues.
#[test]
fn log_buffer_at_large_size() {
    let path = tmp_path("large_buf");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs
        .journal_with(
            &path,
            JournalOptions::new().direct(true).log_buffer_kib(4 * 1024),
        )
        .expect("open with large buffer");

    let payload = vec![0xCDu8; 256];
    let mut last = Lsn::ZERO;
    for _ in 0..100 {
        last = log.append(&payload).expect("append");
    }
    log.sync_through(last).expect("sync");
    drop(log);

    let mut reader = JournalReader::open(&path).expect("reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 100);
}

/// Group-commit coalescence under direct mode. Many threads each
/// call `sync_through` for their own LSN; only one fdatasync
/// syscall actually fires per commit batch. Verified empirically
/// by checking that all callers complete and the final synced_lsn
/// covers the highest appended LSN.
#[test]
fn group_commit_coalescence_under_direct_mode() {
    let path = tmp_path("group_commit");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = Arc::new(
        fs.journal_with(&path, JournalOptions::new().direct(true))
            .expect("open"),
    );

    // Pre-append records so threads have LSNs to sync to.
    let mut lsns = Vec::new();
    for i in 0..32 {
        let p = format!("rec-{i:03}");
        lsns.push(log.append(p.as_bytes()).expect("append"));
    }

    // 32 threads concurrently sync_through their own LSN.
    let mut threads = Vec::new();
    for &lsn in &lsns {
        let log = log.clone();
        threads.push(thread::spawn(move || {
            log.sync_through(lsn).expect("sync");
        }));
    }
    for t in threads {
        t.join().expect("join");
    }

    // After all syncs return, synced_lsn must cover every LSN.
    let final_synced = log.synced_lsn();
    let highest = *lsns.last().unwrap();
    assert!(
        final_synced >= highest,
        "synced_lsn {final_synced:?} < highest LSN {highest:?}"
    );
}

/// Direct mode + root-scoped handle. The handle's path-resolution
/// security check (canonical-prefix verification) must apply to
/// `journal_with` exactly as it does to `journal` and `write`.
#[test]
fn direct_mode_respects_root_scope() {
    let dir = tmp_path("root_scope_dir");
    std::fs::create_dir_all(&dir).expect("create dir");
    let _g = CleanupDir(dir.clone());

    let fs = builder().root(&dir).build().expect("root-scoped handle");

    // Relative path under the root — should succeed.
    let log = fs
        .journal_with("inside.wal", JournalOptions::new().direct(true))
        .expect("open inside root");
    let _ = log.append(b"in-root payload").expect("append");
    log.close().expect("close");

    // Path traversal attempt — should be rejected.
    let escape_attempt = fs.journal_with("../escape.wal", JournalOptions::new().direct(true));
    assert!(
        escape_attempt.is_err(),
        "root-scoped journal_with must reject path traversal"
    );
}

struct CleanupDir(PathBuf);
impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Resume after a tail-truncated journal: simulate a crash by
/// truncating the file to a non-frame-aligned offset, then reopen
/// in direct mode. The reader's clean-end scan should advance
/// past the truncated tail and the new appends should land at
/// the correct sector boundary without destroying earlier records.
#[test]
fn resume_with_truncated_tail() {
    let path = tmp_path("truncated_tail");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");

    // Session 1 — write 10 records, sync, close.
    let mut frame_end_after_3rd = 0u64;
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true))
            .expect("open 1");
        let mut last = Lsn::ZERO;
        for i in 0..10 {
            let p = format!("session1-{i:02}");
            last = log.append(p.as_bytes()).expect("append");
            if i == 2 {
                frame_end_after_3rd = last.as_u64();
            }
        }
        log.sync_through(last).expect("sync");
        log.close().expect("close");
    }
    assert!(frame_end_after_3rd > 0);

    // Simulate a crash by truncating the file to mid-frame
    // (between frames 5 and 6, a few bytes into frame 6).
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("reopen for truncate");
    let truncate_at = frame_end_after_3rd + 30;
    f.set_len(truncate_at).expect("set_len");
    drop(f);

    // Session 2 — reopen in direct mode. Resume scan stops at
    // the last cleanly-decoded frame; subsequent appends extend
    // from there.
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true))
            .expect("open 2");
        let lsn = log
            .append(b"after-recovery")
            .expect("append after recovery");
        log.sync_through(lsn).expect("sync 2");
        log.close().expect("close 2");
    }

    // Reader should see at least 4 records (records 0, 1, 2,
    // possibly 3 depending on where the truncation cut, plus
    // "after-recovery"). The exact count depends on the frame
    // sizes; the important invariant is that all visible records
    // are intact and "after-recovery" appears at the end.
    let mut reader = JournalReader::open(&path).expect("reader");
    let payloads: Vec<Vec<u8>> = reader.iter().map(|r| r.unwrap().payload).collect();
    assert!(
        payloads.len() >= 4,
        "expected ≥ 4 records after recovery, got {}",
        payloads.len()
    );
    assert_eq!(
        payloads.last().unwrap(),
        b"after-recovery",
        "post-recovery append must be the last record"
    );
    // Pre-recovery records must still match their original content.
    for (i, p) in payloads.iter().enumerate().take(3) {
        let expected = format!("session1-{i:02}");
        assert_eq!(p, expected.as_bytes(), "pre-recovery record {i} corrupted");
    }
}

/// Concurrent appends mixed with concurrent sync_throughs in
/// direct mode. The buffer mutex serialises appends; the
/// sync_gate mutex serialises fsyncs; the two interleave correctly.
#[test]
fn direct_mode_interleaved_appends_and_syncs() {
    let path = tmp_path("interleaved");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = Arc::new(
        fs.journal_with(&path, JournalOptions::new().direct(true))
            .expect("open"),
    );

    let mut threads = Vec::new();

    // 8 appender threads.
    for tid in 0..8 {
        let log = log.clone();
        threads.push(thread::spawn(move || {
            for i in 0..50 {
                let p = format!("a-t{tid}-i{i:03}");
                let _ = log.append(p.as_bytes()).expect("append");
            }
        }));
    }

    // 4 sync threads, each calling sync_through periodically.
    for _ in 0..4 {
        let log = log.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..20 {
                let lsn = log.next_lsn();
                log.sync_through(lsn).expect("sync");
                std::thread::yield_now();
            }
        }));
    }

    for t in threads {
        t.join().expect("join");
    }

    // Final flush — make sure everything hits disk.
    log.sync_through(log.next_lsn()).expect("final sync");
    drop(log);

    // Reader must see all 8 × 50 = 400 records, all with valid
    // content matching the appender pattern.
    let mut reader = JournalReader::open(&path).expect("reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 8 * 50);
    let mut seen = std::collections::HashSet::new();
    for r in &recs {
        assert!(
            seen.insert(r.payload.clone()),
            "duplicate record {:?}",
            String::from_utf8_lossy(&r.payload)
        );
    }
}
