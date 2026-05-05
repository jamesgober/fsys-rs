//! Integration tests for Direct-IO journal mode (0.9.0 R-2).
//!
//! Exercises the [`JournalOptions::direct(true)`] path: sector-
//! aligned log-buffer architecture, partial-flush semantics on
//! `sync_through`, buffer-full real flush, oversize-record path,
//! resume-after-clean-shutdown, and reader zero-pad-skipping.
//!
//! On platforms where the filesystem rejects `O_DIRECT` (tmpfs,
//! some FUSE / network mounts, certain CIFS configurations) these
//! tests transparently exercise the buffered fallback path —
//! `JournalHandle::is_direct_active()` reports the true state.
//!
//! All tests use `std::env::temp_dir()`; on Windows that's typically
//! `%TEMP%` (NTFS, supports `FILE_FLAG_NO_BUFFERING`); on Linux
//! `/tmp` (often tmpfs, `O_DIRECT` rejected → fallback path
//! exercised). Both paths produce identical observable behaviour.

use fsys::{builder, JournalOptions, JournalReader};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_journal_direct_{}_{}_{tag}",
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

#[test]
fn direct_journal_round_trip_single_record() {
    let path = tmp_path("single");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true))
        .expect("open direct journal");

    let lsn = log.append(b"hello, direct world").expect("append");
    log.sync_through(lsn).expect("sync");
    drop(log);

    // Reader sees the record.
    let mut reader = JournalReader::open(&path).expect("open reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].payload, b"hello, direct world");
}

#[test]
fn direct_journal_many_small_records_round_trip() {
    let path = tmp_path("many_small");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true))
        .expect("open direct journal");

    let mut last_lsn = fsys::Lsn::ZERO;
    for i in 0..500 {
        let payload = format!("record-{i:04}");
        last_lsn = log.append(payload.as_bytes()).expect("append");
    }
    log.sync_through(last_lsn).expect("sync");
    drop(log);

    let mut reader = JournalReader::open(&path).expect("open reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 500);
    for (i, r) in recs.iter().enumerate() {
        let expect = format!("record-{i:04}");
        assert_eq!(r.payload, expect.as_bytes());
    }
}

#[test]
fn direct_journal_record_spanning_buffer_capacity() {
    // 16 KiB record into a 4 KiB log buffer — exercises the
    // oversize-record direct-write path.
    let path = tmp_path("oversize");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true).log_buffer_kib(4))
        .expect("open");

    let big = vec![0xCDu8; 16 * 1024];
    let lsn1 = log.append(&big).expect("oversize append");
    let lsn2 = log.append(b"after oversize").expect("normal append");
    log.sync_through(lsn2).expect("sync");
    drop(log);

    let mut reader = JournalReader::open(&path).expect("open reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 2, "expected 2 records, got {}", recs.len());
    assert_eq!(recs[0].payload.len(), 16 * 1024);
    assert!(recs[0].payload.iter().all(|&b| b == 0xCD));
    assert_eq!(recs[1].payload, b"after oversize");
    // LSN ordering preserved.
    assert!(recs[0].lsn < recs[1].lsn);
    let _ = lsn1; // captured for the trace if it ever fails
}

#[test]
fn direct_journal_resume_after_clean_close() {
    let path = tmp_path("resume_clean");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");

    // First session: append 100 records, sync, close.
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true))
            .expect("open 1");
        let mut last = fsys::Lsn::ZERO;
        for i in 0..100 {
            let p = format!("session-1-rec-{i:03}");
            last = log.append(p.as_bytes()).expect("append");
        }
        log.close().expect("close");
        let _ = last;
    }

    // Second session: reopen, append 50 more, sync, close.
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true))
            .expect("open 2");
        let mut last = fsys::Lsn::ZERO;
        for i in 0..50 {
            let p = format!("session-2-rec-{i:03}");
            last = log.append(p.as_bytes()).expect("append");
        }
        log.close().expect("close");
        let _ = last;
    }

    // Reader sees both sessions in order.
    let mut reader = JournalReader::open(&path).expect("open reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 150);
    for (i, r) in recs.iter().enumerate().take(100) {
        let expect = format!("session-1-rec-{i:03}");
        assert_eq!(r.payload, expect.as_bytes(), "record {i} mismatch");
    }
    for (i, r) in recs.iter().enumerate().skip(100).take(50) {
        let expect = format!("session-2-rec-{:03}", i - 100);
        assert_eq!(r.payload, expect.as_bytes(), "record {i} mismatch");
    }
}

#[test]
fn direct_journal_concurrent_appends_serialised() {
    // 16 threads × 200 records. Direct-mode appends serialise
    // through the buffer mutex; the test verifies that all 3200
    // records arrive intact and in some legal interleaving.
    let path = tmp_path("concurrent");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = Arc::new(
        fs.journal_with(&path, JournalOptions::new().direct(true))
            .expect("open"),
    );

    let mut threads = Vec::new();
    for tid in 0..16usize {
        let log = log.clone();
        threads.push(std::thread::spawn(move || {
            for r in 0..200usize {
                let p = format!("t{tid:02}-r{r:03}");
                let _ = log.append(p.as_bytes()).expect("concurrent append");
            }
        }));
    }
    for t in threads {
        t.join().expect("join");
    }
    log.sync_through(log.next_lsn()).expect("final sync");
    let final_lsn = log.next_lsn();
    drop(log);

    let mut reader = JournalReader::open(&path).expect("open reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(
        recs.len(),
        16 * 200,
        "expected 3200 records, got {} (final LSN was {})",
        recs.len(),
        final_lsn
    );

    // Every (tid, r) pair appears exactly once.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &recs {
        let s = String::from_utf8_lossy(&r.payload).to_string();
        assert!(seen.insert(s.clone()), "duplicate record: {s}");
    }
    for tid in 0..16 {
        for rec in 0..200 {
            let key = format!("t{tid:02}-r{rec:03}");
            assert!(seen.contains(&key), "missing record: {key}");
        }
    }
}

#[test]
fn direct_journal_empty_record_round_trip() {
    let path = tmp_path("empty_record");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true))
        .expect("open");

    let _ = log.append(b"prefix").expect("append");
    let _ = log.append(b"").expect("empty append");
    let lsn3 = log.append(b"suffix").expect("append");
    log.sync_through(lsn3).expect("sync");
    drop(log);

    let mut reader = JournalReader::open(&path).expect("open reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 3);
    assert_eq!(recs[0].payload, b"prefix");
    assert_eq!(recs[1].payload, b"");
    assert_eq!(recs[2].payload, b"suffix");
}

#[test]
fn direct_mode_observable_via_is_direct_active() {
    let path = tmp_path("observe");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true))
        .expect("open");

    // On most platforms (NTFS on Windows, ext4/xfs/btrfs on Linux,
    // APFS on macOS) Direct-IO is supported and `is_direct_active`
    // returns true. On tmpfs (Linux /tmp on many configurations)
    // O_DIRECT is rejected and `is_direct_active` returns false.
    // Either is a valid outcome; we just confirm the API works.
    let _ = log.is_direct_active();
}

#[test]
fn direct_journal_partial_flush_then_buffer_full_flush() {
    // Validates the load-bearing invariant: a partial flush
    // (sync_through) does NOT advance flush_pos; a subsequent
    // buffer-full flush correctly overwrites the partial-sector
    // pad with new record bytes.
    let path = tmp_path("partial_then_full");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true).log_buffer_kib(4))
        .expect("open");

    // Append a small record, sync (partial flush, zero-pads).
    let lsn1 = log.append(b"small").expect("a1");
    log.sync_through(lsn1).expect("s1");

    // Append enough records to fill the 4 KiB buffer past
    // capacity, triggering a real flush. With ~17-byte frames
    // (5-byte payload + 12 overhead), need ~241 records to fill.
    let mut last = fsys::Lsn::ZERO;
    for i in 0..300 {
        let p = format!("rec-{i:04}");
        last = log.append(p.as_bytes()).expect("append");
    }
    log.sync_through(last).expect("final sync");
    drop(log);

    let mut reader = JournalReader::open(&path).expect("open reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    // Expect: 1 small + 300 numbered = 301 records.
    assert_eq!(recs.len(), 301);
    assert_eq!(recs[0].payload, b"small");
    for (i, r) in recs.iter().enumerate().skip(1) {
        let expect = format!("rec-{:04}", i - 1);
        assert_eq!(r.payload, expect.as_bytes());
    }
}

#[test]
fn buffered_and_direct_journals_are_format_compatible() {
    // A buffered-mode write followed by a direct-mode reopen
    // (and vice versa) is round-trip-safe: the on-disk format is
    // identical, so the reader can decode either.
    let path = tmp_path("compat");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");

    // Phase 1: buffered writes.
    {
        let log = fs.journal(&path).expect("buffered open");
        let _ = log.append(b"buffered-1").unwrap();
        let lsn = log.append(b"buffered-2").unwrap();
        log.sync_through(lsn).unwrap();
        log.close().unwrap();
    }
    // Phase 2: direct reopen + appends.
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true))
            .expect("direct reopen");
        let _ = log.append(b"direct-1").unwrap();
        let lsn = log.append(b"direct-2").unwrap();
        log.sync_through(lsn).unwrap();
        log.close().unwrap();
    }
    // Phase 3: buffered reopen + read.
    let mut reader = JournalReader::open(&path).expect("reader");
    let recs: Vec<_> = reader.iter().map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 4);
    assert_eq!(recs[0].payload, b"buffered-1");
    assert_eq!(recs[1].payload, b"buffered-2");
    assert_eq!(recs[2].payload, b"direct-1");
    assert_eq!(recs[3].payload, b"direct-2");
}
