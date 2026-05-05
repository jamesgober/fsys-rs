//! Crash-safety integration tests for the journal substrate.
//!
//! Spawns a victim subprocess that:
//! 1. Opens a journal (buffered or direct mode).
//! 2. Appends `SYNCED_COUNT` records and calls `sync_through` so
//!    they reach stable storage.
//! 3. Signals `__FSYS_CRASH_BEGIN__` to the parent.
//! 4. Appends more records WITHOUT syncing (these are durability-
//!    not-guaranteed by the journal contract).
//! 5. Signals `__FSYS_CRASH_END__` once N total records appended.
//!
//! The parent kills the victim mid-burst (after `BEGIN`, before
//! `END`), then reopens the journal in the parent and scans:
//!
//! - **Durability invariant.** All `SYNCED_COUNT` synced records
//!   MUST be present and intact (correct payloads, monotonic LSNs).
//! - **Tail-truncation invariant.** Records past `SYNCED_COUNT`
//!   may or may not be present, but the reader MUST detect any
//!   torn frame at the tail via `JournalTailState` (clean end,
//!   truncated header, truncated payload, or checksum mismatch —
//!   all are recoverable). It MUST NOT surface a torn frame as a
//!   valid record (that would be a critical durability bug).
//!
//! Tests both `JournalOptions::default()` (buffered/lock-free) and
//! `JournalOptions::direct(true)` (sector-aligned log buffer).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use fsys::{builder, JournalHandle, JournalOptions, JournalReader, JournalTailState, Lsn};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const ENV_VICTIM: &str = "FSYS_CRASH_JOURNAL_VICTIM";
const ENV_TARGET: &str = "FSYS_CRASH_JOURNAL_TARGET";
const ENV_DIRECT: &str = "FSYS_CRASH_JOURNAL_DIRECT";

const MARKER_BEGIN: &str = "__FSYS_JCRASH_BEGIN__";
const MARKER_END: &str = "__FSYS_JCRASH_END__";

const SYNCED_COUNT: usize = 50;
const TOTAL_TARGET: usize = 500;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_crash_journal_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn record_payload(i: usize) -> Vec<u8> {
    // Distinct, decodable payload per record so we can verify
    // ordering and detect any duplicates after recovery.
    format!("crash-record-{i:08}-payload-bytes").into_bytes()
}

/// Victim-mode entry point. If the env var is set, run the victim
/// work and exit. Otherwise return so the test can continue as
/// parent.
fn maybe_run_as_victim_and_exit() {
    if std::env::var(ENV_VICTIM).is_err() {
        return;
    }
    let target = match std::env::var(ENV_TARGET).ok() {
        Some(s) => PathBuf::from(s),
        None => std::process::exit(101),
    };
    let direct = std::env::var(ENV_DIRECT).ok().as_deref() == Some("1");

    let fs = match builder().build() {
        Ok(h) => h,
        Err(_) => std::process::exit(102),
    };
    let log: Arc<JournalHandle> = if direct {
        match fs.journal_with(&target, JournalOptions::new().direct(true)) {
            Ok(j) => Arc::new(j),
            Err(_) => std::process::exit(103),
        }
    } else {
        match fs.journal(&target) {
            Ok(j) => Arc::new(j),
            Err(_) => std::process::exit(104),
        }
    };

    // 1. Append SYNCED_COUNT records, sync_through.
    let mut last_synced = Lsn::ZERO;
    for i in 0..SYNCED_COUNT {
        last_synced = match log.append(&record_payload(i)) {
            Ok(l) => l,
            Err(_) => std::process::exit(110),
        };
    }
    if log.sync_through(last_synced).is_err() {
        std::process::exit(111);
    }

    // 2. Signal BEGIN — synced records are now durable.
    println!("{MARKER_BEGIN}");
    let _ = std::io::stdout().flush();

    // 3. Append more records WITHOUT syncing. The parent kills
    //    the victim mid-burst.
    for i in SYNCED_COUNT..TOTAL_TARGET {
        if log.append(&record_payload(i)).is_err() {
            std::process::exit(112);
        }
    }

    // 4. Signal END (parent should kill before this in a normal
    //    crash test; signalling here lets the parent verify that
    //    the run-to-completion variant also recovers cleanly).
    println!("{MARKER_END}");
    let _ = std::io::stdout().flush();

    // 5. Linger briefly so the parent has time to kill. If the
    //    parent doesn't kill, we exit cleanly.
    std::thread::sleep(Duration::from_millis(500));
    std::process::exit(0);
}

fn spawn_victim(
    test_fn_name: &str,
    target: &PathBuf,
    direct: bool,
) -> (Child, BufReader<std::process::ChildStdout>) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(&exe);
    cmd.arg("--exact").arg(test_fn_name);
    cmd.arg("--nocapture");
    cmd.env(ENV_VICTIM, "1");
    cmd.env(ENV_TARGET, target);
    cmd.env(ENV_DIRECT, if direct { "1" } else { "0" });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn victim");
    let stdout = child.stdout.take().expect("child stdout");
    let reader = BufReader::new(stdout);
    (child, reader)
}

fn read_until_begin(reader: &mut BufReader<std::process::ChildStdout>) -> bool {
    for line in reader.lines() {
        match line {
            Ok(l) if l.contains(MARKER_BEGIN) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
    false
}

fn run_crash_test(test_fn: &str, direct: bool) {
    maybe_run_as_victim_and_exit();
    let target = tmp(test_fn);
    let _g = Cleanup(target.clone());

    let (mut child, mut reader) = spawn_victim(test_fn, &target, direct);
    let begin = read_until_begin(&mut reader);
    assert!(begin, "victim never signalled BEGIN");

    // Brief jitter so the victim is mid-burst when killed.
    std::thread::sleep(Duration::from_millis(5));

    let _ = child.kill();
    let _ = child.wait();

    // Now reopen the journal in the parent and verify recovery.
    verify_recovery(&target);
}

fn verify_recovery(target: &Path) {
    let mut reader = JournalReader::open(target).expect("open journal for recovery");
    let mut decoded: Vec<Vec<u8>> = Vec::new();
    let mut last_lsn = Lsn::ZERO;
    let mut decode_error: Option<String> = None;
    {
        let mut iter_iter = reader.iter();
        for rec in iter_iter.by_ref() {
            match rec {
                Ok(r) => {
                    assert!(
                        r.lsn >= last_lsn,
                        "non-monotonic LSN: {:?} < {:?}",
                        r.lsn,
                        last_lsn
                    );
                    last_lsn = r.lsn;
                    decoded.push(r.payload);
                }
                Err(e) => {
                    decode_error = Some(format!("{e:?}"));
                    break;
                }
            }
        }
    }
    let tail = reader.tail_state();

    // Durability invariant: at least SYNCED_COUNT synced records
    // must be present and intact.
    assert!(
        decoded.len() >= SYNCED_COUNT,
        "durability violation: only {} records present, need ≥ {} (tail = {:?}, decode error = {:?})",
        decoded.len(),
        SYNCED_COUNT,
        tail,
        decode_error,
    );
    for (i, rec) in decoded.iter().enumerate().take(SYNCED_COUNT) {
        let expected = record_payload(i);
        assert_eq!(rec, &expected, "record {i} corrupted: durability violation");
    }

    // Tail-state invariant: must be one of the recoverable
    // states (clean end, truncated, checksum mismatch). BadMagic
    // / LengthOverflow would indicate format corruption, which
    // is NOT what a normal crash should produce.
    let recoverable = matches!(
        tail,
        JournalTailState::CleanEnd
            | JournalTailState::TruncatedHeader
            | JournalTailState::TruncatedPayload
            | JournalTailState::ChecksumMismatch
    );
    assert!(
        recoverable,
        "non-recoverable tail state {tail:?} after crash — format corruption (decoded {} records)",
        decoded.len()
    );

    // Records past SYNCED_COUNT may or may not be present, but
    // any that ARE present must match their expected content
    // (no torn frames surfaced as records — that's the load-
    // bearing safety invariant).
    for (i, rec) in decoded.iter().enumerate().skip(SYNCED_COUNT) {
        let expected = record_payload(i);
        assert_eq!(
            rec, &expected,
            "record {i} (past sync barrier) has wrong content — torn frame surfaced as valid record!"
        );
    }
}

#[test]
fn crash_journal_buffered_mid_unsynced_burst() {
    run_crash_test("crash_journal_buffered_mid_unsynced_burst", false);
}

#[test]
fn crash_journal_direct_mid_unsynced_burst() {
    run_crash_test("crash_journal_direct_mid_unsynced_burst", true);
}
