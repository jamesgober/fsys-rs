//! # Journal substrate — high-throughput append-only durable log
//!
//! The journal is the WAL/append primitive that databases,
//! queues, and ledgers should use for high-throughput durable
//! writes. Unlike `Handle::write` (atomic-replace, 5–7 syscalls
//! per call, fsync per call → ~500 ops/s), the journal:
//!
//! - Opens once.
//! - Supports concurrent appends without per-call fsync.
//! - Group-commits durability via `sync_through(lsn)`.
//!
//! This example demonstrates the canonical "append many records,
//! sync at a transaction boundary" pattern and shows the
//! throughput delta vs atomic-replace at the same workload.
//!
//! Run: `cargo run --example 17_journal_basics --release`

use fsys::{builder, Lsn};
use std::sync::Arc;
use std::time::Instant;

const RECORDS: usize = 1000;

fn main() -> fsys::Result<()> {
    let fs = builder().build()?;
    let dir = std::env::temp_dir();

    // ──────────────────────────────────────────────────────────
    // Step 1 — open the journal.
    // ──────────────────────────────────────────────────────────
    let journal_path = dir.join("fsys_example_journal.wal");
    let _ = std::fs::remove_file(&journal_path); // start fresh
    let log = Arc::new(fs.journal(&journal_path)?);

    println!("Journal opened at {}", journal_path.display());
    println!("  next_lsn:   {}", log.next_lsn());
    println!("  synced_lsn: {}", log.synced_lsn());
    println!();

    // ──────────────────────────────────────────────────────────
    // Step 2 — append many records WITHOUT fsync per call.
    // Each `append` returns the LSN immediately past the record.
    // ──────────────────────────────────────────────────────────
    let payload = b"a typical row-write record (around 64 bytes)__";
    let t = Instant::now();
    let mut last_lsn = Lsn::ZERO;
    for i in 0..RECORDS {
        last_lsn = log.append(payload)?;
        if i == 0 {
            println!("First append: LSN = {}", last_lsn);
        }
    }
    let append_time = t.elapsed();
    println!("After {RECORDS} appends:");
    println!(
        "  next_lsn:   {} (= {} bytes appended)",
        log.next_lsn(),
        log.next_lsn().as_u64()
    );
    println!(
        "  synced_lsn: {} (still 0 — no fsync yet)",
        log.synced_lsn()
    );
    println!(
        "  append phase: {:.2} ms ({:.0} ns/append)",
        append_time.as_secs_f64() * 1_000.0,
        append_time.as_nanos() as f64 / RECORDS as f64
    );
    println!();

    // ──────────────────────────────────────────────────────────
    // Step 3 — group-commit the entire batch with one fsync.
    // ──────────────────────────────────────────────────────────
    let t = Instant::now();
    log.sync_through(last_lsn)?;
    let sync_time = t.elapsed();
    println!("After sync_through({last_lsn}):");
    println!("  synced_lsn: {} (durable)", log.synced_lsn());
    println!(
        "  sync phase: {:.2} ms (one fsync covers all {RECORDS} records)",
        sync_time.as_secs_f64() * 1_000.0
    );
    println!();

    // ──────────────────────────────────────────────────────────
    // Step 4 — compare to atomic-replace at the same payload.
    // ──────────────────────────────────────────────────────────
    let scratch = dir.join("fsys_example_atomic_replace.bin");
    let _ = std::fs::remove_file(&scratch);
    let t = Instant::now();
    for _ in 0..RECORDS {
        fs.write(&scratch, payload)?;
    }
    let atomic_time = t.elapsed();
    println!("Comparison: same {RECORDS} writes via Handle::write (atomic-replace):");
    println!(
        "  total time: {:.2} ms ({:.0} µs/write)",
        atomic_time.as_secs_f64() * 1_000.0,
        atomic_time.as_micros() as f64 / RECORDS as f64
    );
    println!();

    let journal_total = append_time + sync_time;
    println!(
        "Journal vs atomic-replace speedup: {:.1}× faster",
        atomic_time.as_secs_f64() / journal_total.as_secs_f64()
    );
    println!();

    // ──────────────────────────────────────────────────────────
    // Step 5 — explicit close (final sync + close file).
    // ──────────────────────────────────────────────────────────
    let log =
        Arc::try_unwrap(log).map_err(|_| fsys::Error::Io(std::io::Error::other("unwrap arc")))?;
    log.close()?;

    // Cleanup.
    let _ = std::fs::remove_file(&journal_path);
    let _ = std::fs::remove_file(&scratch);
    Ok(())
}
