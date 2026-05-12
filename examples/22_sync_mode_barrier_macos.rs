//! # `JournalOptions::sync_mode(SyncMode::Barrier)` — macOS F_BARRIERFSYNC
//!
//! 0.9.4 added an opt-in cheaper sync primitive for macOS journals:
//! `F_BARRIERFSYNC`. On Apple Silicon NVMe it is **10–100× cheaper**
//! than `F_FULLFSYNC` (the default) because it returns when writes
//! have reached the device's volatile cache without waiting for the
//! cache to flush to media.
//!
//! ## Crash-safety contract
//!
//! `SyncMode::Barrier` is crash-safe **only** under one of:
//!
//! 1. **PLP-equipped drives.** Enterprise NVMe with power-loss
//!    protection capacitors: the controller acknowledges the cache
//!    flush internally, so a power loss between `Barrier` and the
//!    next media flush is safe.
//!
//! 2. **Explicit eventual-`Full`-sync discipline.** The journal uses
//!    `SyncMode::Barrier` for per-commit barriers but periodically
//!    issues a `SyncMode::Full` at checkpoint boundaries. Records
//!    between checkpoints may be lost on power-loss, but the
//!    application's checkpoint frequency bounds the loss window.
//!
//! On non-PLP consumer NVMe **without** checkpoint discipline, a
//! power loss between `Barrier` sync and the next cache flush can
//! lose the most recent records. `SyncMode::Full` (default) is
//! universally crash-safe.
//!
//! ## Linux + Windows
//!
//! `SyncMode::Barrier` is a no-op on Linux and Windows; their
//! defaults already provide barrier-grade durability (fdatasync,
//! FlushFileBuffers). This example demonstrates the API; the
//! actual cheaper primitive only fires on macOS.
//!
//! ## When to use this pattern
//!
//! macOS journal workloads on PLP-equipped enterprise NVMe where
//! per-commit fsync latency dominates the workload. Most consumer
//! Apple Silicon Macs lack PLP — the default `SyncMode::Full` is
//! the right choice there.
//!
//! Run: `cargo run --example 22_sync_mode_barrier_macos`

use std::sync::Arc;

fn main() -> fsys::Result<()> {
    let path = std::env::temp_dir().join("fsys_example_sync_mode_barrier.wal");
    let _ = std::fs::remove_file(&path);

    let fs = Arc::new(fsys::builder().build()?);

    // Open the journal with SyncMode::Barrier opted in. On macOS
    // this changes sync_through's behavior; elsewhere it's a no-op.
    let opts = fsys::JournalOptions::new().sync_mode(fsys::SyncMode::Barrier);
    let log = fs.journal_with(&path, opts)?;

    // Append + sync — on macOS this fsync uses F_BARRIERFSYNC.
    for i in 0..1000 {
        log.append(format!("event {i:04}").as_bytes())?;
    }
    let lsn = log.next_lsn();
    log.sync_through(lsn)?;

    println!("appended 1000 records + 1 Barrier sync");
    println!("durable through LSN {lsn}");
    println!(
        "platform note: F_BARRIERFSYNC fires only on macOS; \
         Linux + Windows treat this as a no-op (defaults are already barrier-grade)"
    );

    // For belt-and-braces durability — and for production WAL
    // workloads using SyncMode::Barrier — issue a SyncMode::Full
    // at checkpoint boundaries. (This example doesn't construct
    // a second handle just for the demo; the pattern is to open
    // with SyncMode::Full for the checkpoint-issuing handle.)

    log.close()?;
    let _ = std::fs::remove_file(&path);
    Ok(())
}
