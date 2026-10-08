//! Data-integrity regression tests for the journal (1.1.1).
//!
//! Each test pins one failure mode found in the 1.1.0 journal:
//! overlapping LSNs from concurrent oversize Direct-IO appends,
//! zero gaps left by Direct-IO slot rotation, and the reopen
//! paths that could not read past them. All tests go through the
//! public API, run in both Direct-IO and buffered mode where the
//! failure applies, and verify the on-disk result with
//! [`JournalReader`].
//!
//! On filesystems that reject `O_DIRECT` (tmpfs and some FUSE
//! mounts) `direct(true)` silently falls back to buffered IO; the
//! tests still pass there, they just exercise the buffered path.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use fsys::{builder, JournalOptions, JournalReader, JournalTailState};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_journal_integrity_{}_{}_{tag}",
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

/// Reads every record, returning `(lsn, payload)` pairs plus the
/// tail state. Panics on an iterator error.
fn read_all(path: &Path) -> (Vec<(u64, Vec<u8>)>, JournalTailState) {
    let mut reader = JournalReader::open(path).expect("open reader");
    let records = reader
        .iter()
        .map(|r| {
            let r = r.expect("record");
            (r.lsn.as_u64(), r.payload)
        })
        .collect();
    (records, reader.tail_state())
}

fn payloads(path: &Path) -> Vec<Vec<u8>> {
    let (records, state) = read_all(path);
    assert_eq!(
        state,
        JournalTailState::CleanEnd,
        "journal must end cleanly"
    );
    records.into_iter().map(|(_, p)| p).collect()
}

/// FS-J1: concurrent appenders racing an oversize Direct-IO
/// append were handed overlapping LSN ranges and overwrote its
/// bytes. Every acknowledged range must be disjoint and every
/// record must read back intact.
#[test]
fn test_direct_concurrent_oversize_appends_never_overlap() {
    let fs = builder().build().expect("handle");
    for round in 0..3 {
        let path = tmp_path("oversize_race");
        let _g = Cleanup(path.clone());
        let log = Arc::new(
            fs.journal_with(&path, JournalOptions::new().direct(true).log_buffer_kib(4))
                .expect("open direct journal"),
        );
        let mut handles = Vec::new();
        for t in 0..4u8 {
            let log = Arc::clone(&log);
            handles.push(std::thread::spawn(move || {
                let mut out = Vec::new();
                for i in 0..500u32 {
                    let len = if t == 0 && i % 4 == 0 { 6000 } else { 20 };
                    let mut payload = vec![t; len];
                    payload[..4].copy_from_slice(&i.to_le_bytes());
                    let end = log.append(&payload).expect("append").as_u64();
                    out.push((end - (len as u64 + 12), end, payload));
                }
                out
            }));
        }
        let mut acked: Vec<(u64, u64, Vec<u8>)> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        log.sync_through(log.next_lsn()).expect("sync");
        drop(log);

        acked.sort_by_key(|r| r.0);
        for pair in acked.windows(2) {
            assert!(
                pair[1].0 >= pair[0].1,
                "round {round}: LSN ranges overlap: {:?} and {:?}",
                (pair[0].0, pair[0].1),
                (pair[1].0, pair[1].1)
            );
        }
        let (records, state) = read_all(&path);
        assert_eq!(state, JournalTailState::CleanEnd, "round {round}");
        assert_eq!(records.len(), acked.len(), "round {round}: records lost");
        for ((lsn, payload), (start, _, expected)) in records.iter().zip(acked.iter()) {
            assert_eq!(lsn, start, "round {round}");
            assert_eq!(
                payload, expected,
                "round {round}: record at {start} corrupted"
            );
        }
    }
}

/// FS-J2: a Direct-IO slot rotation advanced the file position by
/// the whole slot, leaving a zero gap after the last record. With
/// the default 64 KiB slot and 13-byte frames the gap is 3 bytes
/// at offset 65533; the 1.1.0 reader stopped there with BadMagic
/// and a direct-mode reopen refused the file.
#[test]
fn test_direct_rotation_small_gap_records_readable_and_reopenable() {
    let path = tmp_path("gap3");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let n = 5141usize;
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true))
            .expect("open direct");
        for _ in 0..n {
            let _ = log.append(b"x").expect("append");
        }
        log.close().expect("close");
    }
    let (records, state) = read_all(&path);
    assert_eq!(state, JournalTailState::CleanEnd);
    assert_eq!(records.len(), n);
    for (i, (lsn, _)) in records.iter().enumerate() {
        assert_eq!(*lsn, i as u64 * 13, "record {i} is not contiguous");
    }

    // Reopen in direct mode and keep appending.
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true))
        .expect("direct reopen");
    assert_eq!(log.next_lsn().as_u64(), n as u64 * 13);
    let _ = log.append(b"after-reopen").expect("append");
    log.close().expect("close");
    let all = payloads(&path);
    assert_eq!(all.len(), n + 1);
    assert_eq!(all.last().unwrap(), b"after-reopen");
}

/// FS-J2: a record that does not fit behind a large earlier record
/// left a gap of most of a slot (54 KiB here); the 1.1.0 reader
/// gave up after 8 KiB of zeros and only one record was readable.
#[test]
fn test_direct_rotation_large_gap_all_records_readable() {
    let path = tmp_path("biggap");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    {
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(true))
            .expect("open direct");
        let _ = log.append(&vec![1u8; 10 * 1024]).expect("append 1");
        let _ = log.append(&vec![2u8; 60 * 1024]).expect("append 2");
        let _ = log.append(b"after").expect("append 3");
        log.close().expect("close");
    }
    assert_eq!(
        payloads(&path),
        vec![
            vec![1u8; 10 * 1024],
            vec![2u8; 60 * 1024],
            b"after".to_vec()
        ]
    );
    let log = fs
        .journal_with(&path, JournalOptions::new().direct(true))
        .expect("direct reopen");
    let _ = log.append(b"more").expect("append");
    log.close().expect("close");
    assert_eq!(payloads(&path).len(), 4);
}

/// Mixed record sizes across many rotations in both modes: every
/// record reads back in order with contiguous LSNs.
#[test]
fn test_mixed_sizes_across_rotations_round_trip() {
    for direct in [false, true] {
        let path = tmp_path("mixed");
        let _g = Cleanup(path.clone());
        let fs = builder().build().expect("handle");
        let log = fs
            .journal_with(
                &path,
                JournalOptions::new().direct(direct).log_buffer_kib(4),
            )
            .expect("open");
        let mut expected = Vec::new();
        let mut state = 0x9E37_79B9u32;
        for i in 0..1500u32 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let len = match state % 10 {
                0 => (state as usize >> 8) % 9000,
                1..=3 => (state as usize >> 8) % 600,
                _ => (state as usize >> 8) % 40,
            };
            let payload: Vec<u8> = (0..len).map(|k| (k as u32 ^ i) as u8).collect();
            let _ = log.append(&payload).expect("append");
            expected.push(payload);
            if i % 97 == 0 {
                log.sync_through(log.next_lsn()).expect("sync");
            }
        }
        log.close().expect("close");
        let (records, state) = read_all(&path);
        assert_eq!(state, JournalTailState::CleanEnd, "direct={direct}");
        assert_eq!(records.len(), expected.len(), "direct={direct}");
        let mut next = 0u64;
        for ((lsn, payload), want) in records.iter().zip(expected.iter()) {
            assert_eq!(*lsn, next, "direct={direct}: gap before record");
            assert_eq!(payload, want, "direct={direct}");
            next = lsn + payload.len() as u64 + 12;
        }
    }
}

/// Walks the live journal and returns the end LSN of the longest
/// prefix of cleanly decoded records.
fn clean_prefix_end(path: &Path) -> u64 {
    let mut reader = JournalReader::open(path).expect("open reader");
    let mut end = 0u64;
    for rec in reader.iter() {
        match rec {
            Ok(r) => end = r.lsn.as_u64() + r.payload.len() as u64 + 12,
            Err(_) => break,
        }
    }
    end
}

/// FS-J3: `sync_through` published a durable frontier covering
/// bytes that had not been written yet: in buffered mode a slow
/// appender's reserved range, in Direct-IO mode an append that
/// landed in the log buffer after the leader's flush. Whenever
/// `synced_lsn()` reports a frontier, every byte below it must
/// already decode from the file.
#[test]
fn test_synced_frontier_never_covers_unwritten_bytes() {
    use std::sync::atomic::AtomicBool;
    for direct in [false, true] {
        let path = tmp_path("frontier");
        let _g = Cleanup(path.clone());
        let fs = builder().build().expect("handle");
        let log = Arc::new(
            fs.journal_with(
                &path,
                JournalOptions::new()
                    .direct(direct)
                    .log_buffer_kib(64)
                    .group_commit_window(None),
            )
            .expect("open"),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        // One slow appender with large records (long positioned
        // writes in buffered mode), three fast append+sync loops.
        {
            let log = Arc::clone(&log);
            let stop = Arc::clone(&stop);
            workers.push(std::thread::spawn(move || {
                let big = vec![0xB1u8; 2 * 1024 * 1024];
                for _ in 0..24 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = log.append(&big).expect("big append");
                }
            }));
        }
        for t in 0..3u8 {
            let log = Arc::clone(&log);
            let stop = Arc::clone(&stop);
            workers.push(std::thread::spawn(move || {
                for _ in 0..2000 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let lsn = log.append(&[t; 64]).expect("append");
                    log.sync_through(lsn).expect("sync");
                }
            }));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut checks = 0;
        while std::time::Instant::now() < deadline && !workers.iter().all(|w| w.is_finished()) {
            let synced = log.synced_lsn().as_u64();
            let written = clean_prefix_end(&path);
            assert!(
                written >= synced,
                "direct={direct}: synced_lsn {synced} covers bytes that are not written \
                 (clean prefix ends at {written})"
            );
            checks += 1;
        }
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            w.join().expect("worker");
        }
        assert!(checks > 0);
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).expect("stat").len()
}

/// FS-J5: a buffered reopen set `next_lsn` to the raw file length.
/// After a torn final frame, new appends landed behind the torn
/// bytes and the reader stopped at the tear (ChecksumMismatch), so
/// every record appended after the reopen was unreadable.
#[test]
fn test_buffered_reopen_after_torn_tail_appends_are_readable() {
    let path = tmp_path("torn_reopen");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let first_end;
    {
        let log = fs.journal(&path).expect("open");
        first_end = log.append(b"one").expect("append").as_u64();
        let _ = log.append(b"two-two-two").expect("append");
        log.close().expect("close");
    }
    // Simulate a crash that tore the last frame.
    let len = file_len(&path);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open for truncate")
        .set_len(len - 3)
        .expect("tear");

    let log = fs.journal(&path).expect("reopen");
    assert_eq!(
        log.next_lsn().as_u64(),
        first_end,
        "resume at the last clean frame"
    );
    let _ = log.append(b"three").expect("append");
    log.close().expect("close");
    assert_eq!(payloads(&path), vec![b"one".to_vec(), b"three".to_vec()]);
}

/// Zero bytes after the last record (Direct-IO padding, a crash
/// that extended the file without writing it, or a zero-filling
/// preallocation fallback) must not become the resume point.
#[test]
fn test_reopen_after_zero_tail_resumes_at_last_record() {
    for direct in [false, true] {
        let path = tmp_path("zero_tail");
        let _g = Cleanup(path.clone());
        let fs = builder().build().expect("handle");
        let opts = || JournalOptions::new().direct(direct);
        let end;
        {
            let log = fs.journal_with(&path, opts()).expect("open");
            let _ = log.append(b"alpha").expect("append");
            end = log.append(b"beta").expect("append").as_u64();
            log.close().expect("close");
        }
        let len = file_len(&path);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open")
            .set_len(len + 1024 * 1024)
            .expect("extend with zeros");

        let log = fs.journal_with(&path, opts()).expect("reopen");
        assert_eq!(log.next_lsn().as_u64(), end, "direct={direct}");
        let _ = log.append(b"gamma").expect("append");
        log.close().expect("close");
        assert_eq!(
            payloads(&path),
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()],
            "direct={direct}"
        );
    }
}

/// Crash simulation for concurrent buffered appenders: a reserved
/// range that was never written (zeros) followed by a record that
/// was. The reopen must resume at the hole, not past it, so the
/// journal stays a readable prefix.
#[test]
fn test_reopen_after_unwritten_reservation_hole_resumes_at_hole() {
    let direct = false;
    let path = tmp_path("hole_reopen");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let durable_end;
    {
        let log = fs.journal(&path).expect("open");
        durable_end = log.append(b"durable").expect("append").as_u64();
        log.close().expect("close");
    }
    // 40 zero bytes (the unwritten reservation), then a frame
    // that a later appender did write.
    let mut bytes = std::fs::read(&path).expect("read");
    bytes.resize(bytes.len() + 40, 0);
    {
        let tmp = tmp_path("hole_frame");
        let _g2 = Cleanup(tmp.clone());
        let log = fs.journal(&tmp).expect("frame source");
        let _ = log.append(b"orphan").expect("append");
        log.close().expect("close");
        bytes.extend_from_slice(&std::fs::read(&tmp).expect("read frame"));
    }
    std::fs::write(&path, &bytes).expect("write crashed image");

    let (records, state) = read_all(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(state, JournalTailState::TruncatedHeader);

    let log = fs
        .journal_with(&path, JournalOptions::new().direct(direct))
        .expect("reopen");
    assert_eq!(log.next_lsn().as_u64(), durable_end, "direct={direct}");
    let _ = log.append(b"after-recovery").expect("append");
    log.close().expect("close");
    assert_eq!(
        payloads(&path),
        vec![b"durable".to_vec(), b"after-recovery".to_vec()],
        "direct={direct}"
    );
}

/// A journal whose tail is not recoverable (bad magic) is refused
/// by the buffered open too, instead of appending behind garbage
/// where nothing is readable.
#[test]
fn test_buffered_open_refuses_bad_magic_journal() {
    let path = tmp_path("bad_magic_open");
    let _g = Cleanup(path.clone());
    std::fs::write(&path, b"\xDE\xAD\xBE\xEF\x00\x00\x00\x00garbage").expect("write");
    let fs = builder().build().expect("handle");
    match fs.journal(&path) {
        Err(fsys::Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
        Err(other) => panic!("unexpected error {other:?}"),
        Ok(_) => panic!("opened a journal with a bad magic"),
    }
}

/// FS-J5: preallocation must not change the journal's logical
/// size, in either mode.
#[test]
fn test_preallocate_does_not_change_logical_size() {
    for direct in [false, true] {
        let path = tmp_path("prealloc_size");
        let _g = Cleanup(path.clone());
        let fs = builder().build().expect("handle");
        let opts = || JournalOptions::new().direct(direct);
        let log = fs.journal_with(&path, opts()).expect("open");
        let lsn = log.append(b"one").expect("append");
        log.sync_through(lsn).expect("sync");
        let before = file_len(&path);
        log.preallocate(0, 1024 * 1024).expect("preallocate");
        assert_eq!(file_len(&path), before, "direct={direct}");
        let end = log.append(b"two").expect("append").as_u64();
        log.close().expect("close");
        let log = fs.journal_with(&path, opts()).expect("reopen");
        assert_eq!(log.next_lsn().as_u64(), end, "direct={direct}");
        drop(log);
        assert_eq!(payloads(&path), vec![b"one".to_vec(), b"two".to_vec()]);
    }
}

/// Linux: force the zero-filling `posix_fallocate` fallback (it
/// extends the file) while appenders run, in a child process so the
/// environment variable does not leak into other tests. The logical
/// size must be restored without truncating any concurrent append.
#[cfg(target_os = "linux")]
#[test]
fn test_preallocate_fallback_with_concurrent_appends_keeps_every_record() {
    const CHILD: &str = "FSYS_JOURNAL_INTEGRITY_PREALLOC_CHILD";
    const NAME: &str = "test_preallocate_fallback_with_concurrent_appends_keeps_every_record";
    if std::env::var_os(CHILD).is_none() {
        let exe = std::env::current_exe().expect("current_exe");
        let status = std::process::Command::new(exe)
            .args(["--exact", NAME, "--nocapture", "--test-threads", "1"])
            .env(CHILD, "1")
            .env("FSYS_TEST_FORCE_POSIX_FALLOCATE", "1")
            .status()
            .expect("spawn child");
        assert!(status.success(), "child run failed: {status}");
        return;
    }
    for direct in [false, true] {
        let path = tmp_path("prealloc_race");
        let _g = Cleanup(path.clone());
        let fs = builder().build().expect("handle");
        let log = Arc::new(
            fs.journal_with(
                &path,
                JournalOptions::new().direct(direct).log_buffer_kib(4),
            )
            .expect("open"),
        );
        let mut writers = Vec::new();
        for t in 0..4u8 {
            let log = Arc::clone(&log);
            writers.push(std::thread::spawn(move || {
                let mut out = Vec::new();
                for i in 0..300u32 {
                    let mut p = vec![t; 40 + (i as usize % 300)];
                    p[..4].copy_from_slice(&i.to_le_bytes());
                    let _ = log.append(&p).expect("append");
                    out.push(p);
                }
                out
            }));
        }
        for k in 0..20u64 {
            log.preallocate(0, (k + 1) * 256 * 1024)
                .expect("preallocate");
        }
        let mut expected: Vec<Vec<u8>> = writers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect();
        let end = log.next_lsn().as_u64();
        log.sync_through(log.next_lsn()).expect("sync");
        drop(log);
        let mut got = payloads(&path);
        assert_eq!(got.len(), expected.len(), "direct={direct}");
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "direct={direct}");
        let log = fs
            .journal_with(&path, JournalOptions::new().direct(direct))
            .expect("reopen");
        assert_eq!(log.next_lsn().as_u64(), end, "direct={direct}");
    }
}
