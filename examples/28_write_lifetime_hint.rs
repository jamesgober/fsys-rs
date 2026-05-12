//! # `JournalOptions::write_lifetime_hint(...)` — Linux multi-stream NVMe
//!
//! 0.9.4 added support for the Linux `F_SET_RW_HINT` fcntl, applied
//! at journal-open time. On multi-stream NVMe drives, write hints
//! let the controller cluster writes with similar expected
//! lifetimes into the same NAND erase blocks. The benefit:
//!
//! - **Long-lived data** (journal records, WAL segments) goes into
//!   blocks dedicated to long-lived data.
//! - **Short-lived data** (scratch files, transient caches) goes
//!   into blocks dedicated to short-lived data.
//!
//! When data with similar lifetimes lives together, the NAND
//! garbage collector has less work to do — writes can be erased
//! as whole blocks rather than rewriting half a block to migrate
//! still-live data. This reduces **write amplification** by
//! 2–5× on streaming workloads.
//!
//! ## Available hints
//!
//! - `WriteLifetimeHint::None` (default) — no hint; drive picks.
//! - `WriteLifetimeHint::Short` — data lives < seconds.
//! - `WriteLifetimeHint::Medium` — data lives seconds to minutes.
//! - `WriteLifetimeHint::Long` — data lives minutes to hours.
//!   **Typical journal choice.**
//! - `WriteLifetimeHint::Extreme` — data lives hours to days.
//!   For archival / WORM-style workloads.
//!
//! ## When to use this pattern
//!
//! Linux + NVMe drives that honour `F_SET_RW_HINT`. Common in
//! enterprise / datacenter NVMe; consumer drives often ignore
//! the hint.
//!
//! Use `Long` for production WAL workloads — they're the
//! canonical "long-lived" data in a database, and clustering
//! them away from ephemeral caches measurably reduces GC
//! pressure.
//!
//! ## When NOT to use this pattern
//!
//! - **macOS / Windows.** `F_SET_RW_HINT` is Linux-only; the
//!   knob is a no-op elsewhere.
//! - **Consumer NVMe.** Most don't implement multi-stream
//!   streaming. No harm — the hint is silently ignored.
//! - **Drives without confirmed multi-stream support.** Check
//!   your drive's datasheet; if it doesn't list multi-stream
//!   or stream-directives support, the hint won't help.
//!
//! Run: `cargo run --example 28_write_lifetime_hint`

use std::sync::Arc;

fn main() -> fsys::Result<()> {
    let path = std::env::temp_dir().join("fsys_example_write_lifetime_hint.wal");
    let _ = std::fs::remove_file(&path);

    let fs = Arc::new(fsys::builder().build()?);

    // Open the journal with WriteLifetimeHint::Long — the canonical
    // production choice for WAL workloads. On Linux + multi-stream
    // NVMe this clusters journal data into long-lived NAND blocks;
    // elsewhere it's a no-op.
    let opts = fsys::JournalOptions::new()
        .write_lifetime_hint(Some(fsys::WriteLifetimeHint::Long));
    let log = fs.journal_with(&path, opts)?;

    println!("journal opened with WriteLifetimeHint::Long");
    println!(
        "  expected GC write-amplification reduction: 2-5x on Linux + multi-stream NVMe"
    );
    println!("  no-op on macOS / Windows / consumer NVMe (silent)");

    // Use the journal as you would normally.
    for i in 0..100 {
        log.append(format!("wal entry {i:04}").as_bytes())?;
    }
    let lsn = log.next_lsn();
    log.sync_through(lsn)?;
    println!();
    println!("appended 100 records + 1 sync");
    println!("durable through LSN {lsn}");

    log.close()?;
    let _ = std::fs::remove_file(&path);
    Ok(())
}
