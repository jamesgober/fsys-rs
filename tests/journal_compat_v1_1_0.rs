//! On-disk compatibility with journals written by fsys 1.1.0.
//!
//! The fixtures in `tests/fixtures/journal_v1_1_0/` were written by
//! the 1.1.0 code (commit 03bc1bb) on Linux ext4 with Direct IO
//! active; `generate.rs` in that directory is the generator. The
//! Direct-IO files contain the zero gaps 1.1.0 left at log-buffer
//! slot rotations and at reopen, including gaps shorter than four
//! bytes and longer than 8 KiB, which the 1.1.0 reader itself could
//! not get past. Every record must read back, and both open modes
//! must resume after the last record and append readable records.
//!
//! The expected payloads are rebuilt with the same functions the
//! generator used.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use fsys::{builder, JournalOptions, JournalReader, JournalTailState, Lsn};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn gap1_payload(i: u32) -> Vec<u8> {
    vec![(i % 251) as u8]
}

fn gap3_payload(i: u32) -> Vec<u8> {
    if i == 0 {
        vec![0xA0]
    } else {
        let b = (i % 200) as u8;
        vec![b, b.wrapping_add(1), b.wrapping_add(2)]
    }
}

fn biggap_payloads() -> Vec<Vec<u8>> {
    vec![
        vec![0x11; 100],
        (0..20 * 1024).map(|k| (k % 253) as u8).collect(),
        b"after-big-gap".to_vec(),
    ]
}

fn reopen_payloads() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = (0..20)
        .map(|i| format!("session-1-{i:03}").into_bytes())
        .collect();
    v.extend((0..20).map(|i| format!("session-2-{i:03}").into_bytes()));
    v
}

fn buffered_payloads() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = (0..10)
        .map(|i| format!("single-{i}").into_bytes())
        .collect();
    v.extend((0..10).map(|i| format!("batch-{i}").into_bytes()));
    v
}

/// `(fixture file, expected payloads, written with Direct IO)`.
fn fixtures() -> Vec<(&'static str, Vec<Vec<u8>>, bool)> {
    vec![
        (
            "direct_gap1.wal",
            (0..700).map(gap1_payload).collect(),
            true,
        ),
        (
            "direct_gap3.wal",
            (0..600).map(gap3_payload).collect(),
            true,
        ),
        ("direct_biggap.wal", biggap_payloads(), true),
        ("direct_reopen.wal", reopen_payloads(), true),
        ("buffered.wal", buffered_payloads(), false),
    ]
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("journal_v1_1_0")
        .join(name)
}

static C: AtomicU64 = AtomicU64::new(0);

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Copies a fixture to a scratch path so tests that write never
/// touch the committed file.
fn scratch_copy(name: &str) -> (PathBuf, Cleanup) {
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dst = std::env::temp_dir().join(format!(
        "fsys_journal_compat_{}_{}_{name}",
        std::process::id(),
        n
    ));
    let _ = std::fs::copy(fixture_path(name), &dst).expect("copy fixture");
    (dst.clone(), Cleanup(dst))
}

fn read_all(path: &Path) -> (Vec<(Lsn, Vec<u8>)>, JournalTailState) {
    let mut reader = JournalReader::open(path).expect("open reader");
    let records = reader
        .iter()
        .map(|r| {
            let r = r.expect("record");
            (r.lsn, r.payload)
        })
        .collect();
    (records, reader.tail_state())
}

fn end_of(records: &[(Lsn, Vec<u8>)]) -> u64 {
    records
        .last()
        .map_or(0, |(lsn, p)| lsn.as_u64() + p.len() as u64 + 12)
}

#[test]
fn test_v1_1_0_fixtures_read_every_record() {
    for (name, expected, _) in fixtures() {
        let path = fixture_path(name);
        let (records, state) = read_all(&path);
        assert_eq!(state, JournalTailState::CleanEnd, "{name}");
        let payloads: Vec<Vec<u8>> = records.iter().map(|(_, p)| p.clone()).collect();
        assert_eq!(payloads.len(), expected.len(), "{name}: record count");
        assert_eq!(payloads, expected, "{name}: payloads");
        for pair in records.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{name}: LSNs must increase");
        }
        let mut reader = JournalReader::open(&path).expect("reader");
        for (lsn, payload) in &records {
            assert_eq!(
                &reader.read_at_lsn(*lsn).expect("read_at_lsn").payload,
                payload,
                "{name}: read_at_lsn({lsn})"
            );
        }
    }
}

#[test]
fn test_v1_1_0_direct_fixtures_contain_writer_gaps() {
    // Guard against regenerating the fixtures without the gaps they
    // exist to cover: in every Direct-IO fixture some record must
    // start past the end of the previous one.
    for (name, _, direct) in fixtures() {
        if !direct {
            continue;
        }
        let (records, _) = read_all(&fixture_path(name));
        let gaps = records
            .windows(2)
            .filter(|w| w[0].0.as_u64() + w[0].1.len() as u64 + 12 < w[1].0.as_u64())
            .count();
        assert!(gaps > 0, "{name}: no writer gap between records");
    }
}

#[test]
fn test_v1_1_0_fixtures_reopen_and_append_in_both_modes() {
    let fs = builder().build().expect("handle");
    for (name, expected, _) in fixtures() {
        for direct in [false, true] {
            let (path, _g) = scratch_copy(name);
            let end = end_of(&read_all(&path).0);
            {
                let log = fs
                    .journal_with(&path, JournalOptions::new().direct(direct))
                    .expect("open 1.1.0 journal");
                assert_eq!(
                    log.next_lsn().as_u64(),
                    end,
                    "{name} direct={direct}: resume point"
                );
                let lsn = log.append(b"appended-by-1.1.1").expect("append");
                log.sync_through(lsn).expect("sync");
                log.close().expect("close");
            }
            let (records, state) = read_all(&path);
            assert_eq!(state, JournalTailState::CleanEnd, "{name} direct={direct}");
            let payloads: Vec<Vec<u8>> = records.into_iter().map(|(_, p)| p).collect();
            assert_eq!(payloads.len(), expected.len() + 1, "{name} direct={direct}");
            assert_eq!(
                &payloads[..expected.len()],
                &expected[..],
                "{name} direct={direct}"
            );
            assert_eq!(payloads.last().unwrap(), b"appended-by-1.1.1");
        }
    }
}

#[test]
fn test_v1_1_0_fixtures_torn_last_record_recovers() {
    // Simulated crash on top of a 1.1.0 journal: cut the last record
    // in half. The reader reports a recoverable tail and the reopen
    // resumes right after the previous record.
    let fs = builder().build().expect("handle");
    for (name, expected, _) in fixtures() {
        for direct in [false, true] {
            let (path, _g) = scratch_copy(name);
            let (records, _) = read_all(&path);
            let (last_lsn, last_payload) = records.last().expect("records").clone();
            let cut = last_lsn.as_u64() + (last_payload.len() as u64 + 12) / 2;
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open")
                .set_len(cut)
                .expect("tear");

            let (torn, state) = read_all(&path);
            assert_eq!(torn.len(), expected.len() - 1, "{name}");
            assert!(
                matches!(
                    state,
                    JournalTailState::TruncatedHeader
                        | JournalTailState::TruncatedPayload
                        | JournalTailState::ChecksumMismatch
                ),
                "{name}: tail {state:?}"
            );
            let log = fs
                .journal_with(&path, JournalOptions::new().direct(direct))
                .expect("reopen torn journal");
            assert_eq!(
                log.next_lsn().as_u64(),
                end_of(&torn),
                "{name} direct={direct}"
            );
            let _ = log.append(b"after-tear").expect("append");
            log.close().expect("close");
            let (records, state) = read_all(&path);
            assert_eq!(state, JournalTailState::CleanEnd, "{name} direct={direct}");
            assert_eq!(records.len(), expected.len(), "{name} direct={direct}");
            assert_eq!(records.last().unwrap().1, b"after-tear");
        }
    }
}
