//! Storage-engine integration tests for the journal —
//! preallocate + advise plumbing, plus correctness invariants
//! that database WAL workloads depend on.

use fsys::{builder, Advice};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_journal_storage_{}_{}_{tag}",
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
fn preallocate_succeeds_on_fresh_journal() {
    let path = tmp_path("prealloc_fresh");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs.journal(&path).expect("journal");

    // Reserve 1 MiB.
    log.preallocate(0, 1024 * 1024)
        .expect("preallocate should succeed");
    log.close().expect("close");
}

#[test]
fn preallocate_zero_length_is_noop() {
    let path = tmp_path("prealloc_zero");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs.journal(&path).expect("journal");
    log.preallocate(0, 0)
        .expect("zero-length preallocate is noop");
    log.close().expect("close");
}

#[test]
fn preallocate_then_append_within_reserved_region() {
    // Reserve 100 KiB up-front, then append a few records.
    // The appends should land within the reserved region with
    // no extent-allocation jitter (we don't measure the jitter
    // here — that's a bench concern — but we verify correctness:
    // the appends succeed and read back identically).
    let path = tmp_path("prealloc_then_write");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs.journal(&path).expect("journal");

    log.preallocate(0, 100 * 1024).expect("preallocate");

    let payloads: Vec<Vec<u8>> = (0..10)
        .map(|i| format!("record-{i}").into_bytes())
        .collect();
    let mut last_lsn = fsys::Lsn::ZERO;
    for p in &payloads {
        last_lsn = log.append(p).expect("append");
    }
    log.sync_through(last_lsn).expect("sync");
    log.close().expect("close");

    // Read back via JournalReader.
    let mut reader = fsys::JournalReader::open(&path).expect("reader");
    let read_payloads: Vec<Vec<u8>> = reader.iter().map(|r| r.expect("decode").payload).collect();
    assert_eq!(read_payloads, payloads);
}

#[test]
fn advise_sequential_succeeds() {
    let path = tmp_path("advise_seq");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs.journal(&path).expect("journal");
    let _ = log.append(b"record").expect("append");
    log.advise(0, 0, Advice::Sequential)
        .expect("advise(Sequential) should succeed");
}

#[test]
fn advise_random_succeeds() {
    let path = tmp_path("advise_random");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs.journal(&path).expect("journal");
    let _ = log.append(b"record").expect("append");
    log.advise(0, 0, Advice::Random)
        .expect("advise(Random) should succeed");
}

#[test]
fn advise_will_need_succeeds() {
    let path = tmp_path("advise_will_need");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs.journal(&path).expect("journal");
    let _ = log.append(b"record").expect("append");
    log.advise(0, 100, Advice::WillNeed)
        .expect("advise(WillNeed) should succeed");
}

#[test]
fn advise_dont_need_succeeds() {
    let path = tmp_path("advise_dont_need");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs.journal(&path).expect("journal");
    // Append 10 KiB so DONTNEED has a real range to release.
    let payload = vec![0u8; 10 * 1024];
    let _ = log.append(&payload).expect("append");
    log.advise(0, 0, Advice::DontNeed)
        .expect("advise(DontNeed) should succeed");
}

#[test]
fn advise_normal_succeeds() {
    let path = tmp_path("advise_normal");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");
    let log = fs.journal(&path).expect("journal");
    let _ = log.append(b"record").expect("append");
    log.advise(0, 0, Advice::Normal)
        .expect("advise(Normal) should succeed");
}

/// Demonstrates the canonical storage-engine WAL pattern:
/// 1. Open the journal.
/// 2. Preallocate the expected size (no allocation jitter).
/// 3. Hint sequential access (kernel prefetch).
/// 4. Append many records.
/// 5. Sync at transaction boundary.
/// 6. Replay via JournalReader on subsequent open.
#[test]
fn end_to_end_storage_engine_pattern() {
    let path = tmp_path("e2e_pattern");
    let _g = Cleanup(path.clone());
    let fs = builder().build().expect("handle");

    // Phase 1 — write a transaction.
    {
        let log = fs.journal(&path).expect("journal");
        log.preallocate(0, 64 * 1024).expect("preallocate 64 KiB");
        log.advise(0, 0, Advice::Sequential)
            .expect("advise Sequential");

        let mut last_lsn = fsys::Lsn::ZERO;
        for i in 0..100 {
            let record = format!("txn=1 seq={i:04}").into_bytes();
            last_lsn = log.append(&record).expect("append");
        }
        log.sync_through(last_lsn).expect("commit");
        log.close().expect("close");
    }

    // Phase 2 — replay on fresh open.
    let mut reader = fsys::JournalReader::open(&path).expect("reader");
    reader.advise_sequential().ok(); // best-effort; method may not exist yet
    let count = reader.iter().filter_map(|r| r.ok()).count();
    assert_eq!(count, 100);
    assert_eq!(reader.tail_state(), fsys::JournalTailState::CleanEnd);
}
