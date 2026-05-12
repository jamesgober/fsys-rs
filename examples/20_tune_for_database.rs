//! # `Builder::tune_for(Workload::Database)` — one-line storage-engine preset
//!
//! 0.9.2 introduced workload presets to bundle the multi-knob
//! coordination that storage-engine workloads need. The `Database`
//! preset sets four knobs at once to values tuned for sustained NVMe
//! bulk-write workloads (HiveDB, embedded KV stores, LSM-tree
//! compaction):
//!
//! | Knob | Default (`Workload::Default`) | `Workload::Database` |
//! |---|---:|---:|
//! | `buffer_pool_count` | 64 | 1024 |
//! | `buffer_pool_block_size` | 4 KiB | 8 KiB |
//! | `io_uring_queue_depth` | 128 | 256 |
//! | `batch_queue_max` | 1024 | 4096 |
//!
//! Total resident memory for the pool jumps from 256 KiB to 8 MiB —
//! 32× more — to keep the dispatcher fed without per-op allocation.
//!
//! ## Apply presets first, override individual knobs second
//!
//! `tune_for(...)` mutates four fields at once. Individual setter
//! calls (`buffer_pool_count`, `io_uring_queue_depth`, etc.) override
//! the preset's value for that one knob — but calling `tune_for`
//! AFTER an individual setter overwrites the manual setting. Always
//! apply the preset first, then layer overrides.
//!
//! ## When to use this pattern
//!
//! Any storage-engine workload on NVMe: relational DBs, LSM trees,
//! KV stores, write-ahead logs. The preset is calibrated for
//! sustained sequential append + concurrent batch flush.
//!
//! ## When NOT to use this pattern
//!
//! Single-file or low-rate workloads. The 8 MiB pool footprint is
//! wasted memory for occasional file IO. `Workload::Default`
//! (= no preset) is the right baseline there.
//!
//! Run: `cargo run --example 20_tune_for_database`

use std::sync::Arc;

fn main() -> fsys::Result<()> {
    // Preset-first, then optional overrides. This is the
    // canonical pattern.
    let fs = Arc::new(
        fsys::builder()
            .tune_for(fsys::Workload::Database)
            // After-preset overrides — these layer on top of the
            // preset's values. Example: keep Database's buffer pool
            // sizing but raise the dispatcher to 4 shards.
            .dispatcher_shards(4)
            .build()?,
    );

    println!("handle built with Workload::Database preset");
    println!("  active method: {:?}", fs.active_method());
    println!("  sector size:   {} bytes", fs.sector_size());

    // The preset is wired for sustained workloads — exercise it
    // with a journal append loop and a final group-commit.
    let log_path = std::env::temp_dir().join("fsys_example_tune_for_db.wal");
    let _ = std::fs::remove_file(&log_path);

    let log = fs.journal(&log_path)?;
    let start = std::time::Instant::now();
    for i in 0..10_000 {
        let payload = format!("txn {i:05}: insert/update mixed");
        log.append(payload.as_bytes())?;
    }
    log.sync_through(log.next_lsn())?;
    let elapsed = start.elapsed();

    println!(
        "appended 10,000 records + 1 sync in {elapsed:?} ({:.0} ops/sec)",
        10_000.0 / elapsed.as_secs_f64()
    );
    println!("durable through LSN {}", log.synced_lsn());

    log.close()?;
    let _ = std::fs::remove_file(&log_path);
    Ok(())
}
