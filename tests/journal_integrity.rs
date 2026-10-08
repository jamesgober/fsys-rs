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
