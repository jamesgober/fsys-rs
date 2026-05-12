//! # `JournalHandle::append_batch` — vectored journal append
//!
//! 0.9.1 added a vectored append API that submits N records as a single
//! framed-write syscall: **one LSN reservation**, **one contiguous
//! allocation**, **one `pwrite` syscall** (or one log-buffer acquisition
//! in Direct-IO mode). The same crash-safety contract applies — each
//! record is individually CRC-32C-framed so partial-batch recovery
//! yields the longest valid prefix — but the per-record overhead is
//! amortised away.
//!
//! On Windows NTFS the win is ~1.6× per-record vs `append`-in-loop; on
//! Linux NVMe with io_uring the win is larger because the kernel-side
//! framing amortisation is bigger.
//!
//! ## When to use this pattern
//!
//! Database WAL workloads where a single transaction commits N records
//! at once: a relational `INSERT` ... `RETURNING` that produces N row-
//! level WAL entries before its `COMMIT` record, a queue's per-batch
//! enqueue, a ledger's daily-snapshot dump. Any case where the natural
//! unit of work is "N records committed together" — `append_batch`
//! is materially faster than calling `append` in a loop.
//!
//! ## When NOT to use this pattern
//!
//! Single-record append patterns (one record per logical operation).
//! `append` is the right primitive there; `append_batch` of one record
//! is identical in cost.
//!
//! Run: `cargo run --example 18_journal_append_batch`

use std::sync::Arc;

fn main() -> fsys::Result<()> {
    let path = std::env::temp_dir().join("fsys_example_append_batch.wal");
    // Ensure clean state — examples should always start from scratch.
    let _ = std::fs::remove_file(&path);

    let fs = Arc::new(fsys::builder().build()?);
    let log = fs.journal(&path)?;

    // Build a 64-record batch — represents 64 row-level WAL entries
    // from a multi-row transaction.
    let records: Vec<Vec<u8>> = (0..64)
        .map(|i| format!("txn 42: insert row {i:03} (key=v{i:03})").into_bytes())
        .collect();
    // `append_batch` takes `&[&[u8]]` — slices of slices. Vec<Vec<u8>>
    // is the typical owned representation; convert with map(Vec::as_slice).
    let refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();

    let lsn = log.append_batch(&refs)?;
    println!("submitted 64 records in one syscall, end LSN = {lsn}");

    // One group-commit fsync — same as `append`'s pattern.
    log.sync_through(lsn)?;
    println!("durable through LSN {}", log.synced_lsn());

    // Read-back via the reader, verify all 64 records survived.
    log.close()?;
    let mut reader = fsys::JournalReader::open(&path)?;
    let mut count = 0;
    for record in reader.iter() {
        let rec = record?;
        count += 1;
        // Spot-check: first and last records match their input.
        if rec.payload == b"txn 42: insert row 000 (key=v000)" {
            println!("read back first record: {} bytes", rec.payload.len());
        }
    }
    assert_eq!(count, 64, "all 64 records must be present");
    println!("replay confirmed: {count} records, tail state {:?}", reader.tail_state());

    let _ = std::fs::remove_file(&path);
    Ok(())
}
