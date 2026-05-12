#![no_main]
//! 0.9.7 audit M-7 — fuzz target for the journal append path.
//!
//! Interprets fuzz input as a length-prefixed sequence of records
//! to append to a journal. Verifies:
//!
//! 1. **No panic** for any input shape. Empty records, very
//!    large records, records that span the stack-fast-path
//!    threshold, batches of mixed-size records — all must
//!    surface either `Ok(...)` or a structured `Err(...)`.
//! 2. **Read-back fidelity.** After append + sync + close + reopen,
//!    the journal reader yields the exact same byte sequences in
//!    the same order. Any divergence is a corruption bug.
//! 3. **Tail-state correctness.** A cleanly-closed journal reads
//!    back with `JournalTailState::CleanEnd`. Any other state on
//!    a clean close indicates the writer left a torn frame.
//!
//! ## Why this matters
//!
//! The journal is the load-bearing HiveDB primitive (every WAL
//! write goes through `JournalHandle::append`). Any panic /
//! corruption here breaks every downstream consumer. The
//! existing `journal_frame` target validates the framing format
//! in isolation; this target validates the **end-to-end**
//! append path: encode, reserve LSN, write to file, sync,
//! re-open, decode, verify.

use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path() -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_fuzz_journal_append_{}_{}",
        std::process::id(),
        n
    ))
}

/// Parse the fuzz input as a sequence of length-prefixed records.
/// Each record: 2-byte little-endian length prefix, then that
/// many bytes of payload. Stops at the first malformed prefix.
/// Caps total record count and individual record size to avoid
/// fuzzer-driven memory blowups.
fn parse_records(mut data: &[u8]) -> Vec<&[u8]> {
    const MAX_RECORDS: usize = 64;
    const MAX_RECORD_LEN: usize = 8 * 1024;

    let mut out = Vec::new();
    while out.len() < MAX_RECORDS && data.len() >= 2 {
        let len = u16::from_le_bytes([data[0], data[1]]) as usize;
        data = &data[2..];
        let take = len.min(MAX_RECORD_LEN).min(data.len());
        out.push(&data[..take]);
        data = &data[take..];
    }
    out
}

fuzz_target!(|data: &[u8]| {
    let records = parse_records(data);

    let Ok(fs) = fsys::builder().build() else {
        return;
    };

    let path = tmp_path();
    // Open journal, append all records, sync the last, close.
    let mut last_lsn = None;
    let Ok(log) = fs.journal(&path) else {
        let _ = std::fs::remove_file(&path);
        return;
    };
    for r in &records {
        match log.append(r) {
            Ok(lsn) => last_lsn = Some(lsn),
            Err(_) => {
                // Append rejected — clean error, not a panic.
                let _ = log.close();
                let _ = std::fs::remove_file(&path);
                return;
            }
        }
    }
    if let Some(lsn) = last_lsn {
        let _ = log.sync_through(lsn);
    }
    let _ = log.close();

    // Re-open via reader; verify byte-for-byte read-back.
    if let Ok(mut reader) = fsys::JournalReader::open(&path) {
        let decoded: Vec<Vec<u8>> = reader
            .iter()
            .filter_map(|r| r.ok().map(|rec| rec.payload))
            .collect();
        // Tail must be CleanEnd because we synced + closed cleanly.
        debug_assert_eq!(
            reader.tail_state(),
            fsys::JournalTailState::CleanEnd,
            "clean close must yield CleanEnd tail",
        );
        // Decoded sequence must match what we appended (modulo any
        // appends that failed — but we returned early on Err).
        debug_assert_eq!(
            decoded.len(),
            records.len(),
            "record count divergence — encoder/decoder out of sync",
        );
        for (i, (got, want)) in decoded.iter().zip(records.iter()).enumerate() {
            debug_assert_eq!(
                got.as_slice(),
                *want,
                "record {i} round-trip diverged — frame corruption?",
            );
        }
    }
    let _ = std::fs::remove_file(&path);
});
