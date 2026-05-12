//! # `Builder::dispatcher_shards(N)` — multi-core batch scaling
//!
//! 0.9.3 lifted the pre-0.9.3 single-thread ceiling on the per-handle
//! batch dispatcher. Setting `dispatcher_shards(N)` (where `N > 1`)
//! spawns N independent dispatcher threads per handle; batches are
//! hash-routed by the first op's path so within-batch order is
//! preserved while concurrent submitters writing to different files
//! scale near-linearly with shard count.
//!
//! Default `N = 1` preserves pre-0.9.3 behavior exactly: a single
//! dispatcher thread per handle, one bounded queue, every batch
//! processed serially in submission order.
//!
//! Clamped to `1..=64`. The high cap reflects that more than ~64
//! dispatcher threads per handle is pathological; `num_cpus::get()`
//! is the natural ceiling for any realistic host.
//!
//! ## When to use this pattern
//!
//! Multi-core hosts where a single handle is the throughput
//! bottleneck for **parallel batch submitters writing to different
//! files** (e.g., a database flushing many SST tables concurrently
//! during compaction). On these workloads the pre-0.9.3 single
//! dispatcher was a hard one-core ceiling; `dispatcher_shards =
//! num_cpus::get()` lifts it.
//!
//! ## When NOT to use this pattern
//!
//! - **Single-writer workloads.** No parallelism benefit — one shard
//!   handles all work serially anyway.
//! - **Latency-sensitive workloads.** Each shard has its own time
//!   window, so cross-shard ordering across batches is not
//!   guaranteed (it never was at the pipeline-level, but multi-shard
//!   widens the variability slightly).
//! - **Workloads where batches rarely touch distinct paths.**
//!   Sharding by hash collapses to one shard when all batches target
//!   the same path — no benefit, same as `N = 1`.
//!
//! Aggregate queue depth scales with shard count: with
//! `batch_queue_max(1024)` and `dispatcher_shards(8)`, the pipeline
//! can hold 8 × 1024 = 8 K batches in flight.
//!
//! Run: `cargo run --example 23_dispatcher_shards`

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

fn main() -> fsys::Result<()> {
    let dir = std::env::temp_dir().join("fsys_example_dispatcher_shards");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");

    // 4 dispatcher shards — exercises multi-shard hash routing.
    let fs = Arc::new(fsys::builder().dispatcher_shards(4).build()?);
    println!("handle built with dispatcher_shards(4)");

    // Spawn 8 submitter threads — each writes to a distinct path.
    // With shards=4, batches hash-route across 2 submitters per shard.
    let mut handles = Vec::new();
    let start = std::time::Instant::now();

    for tid in 0..8 {
        let fs = fs.clone();
        let dir = dir.clone();
        handles.push(thread::spawn(move || {
            // Each submitter writes 100 files into its own
            // sub-namespace — distinct path -> distinct shard
            // (by hash) -> parallel dispatcher.
            for i in 0..100 {
                let path: PathBuf = dir.join(format!("t{tid}_f{i:03}.dat"));
                let data = format!("submitter {tid} file {i}");
                fs.write(&path, data.as_bytes()).expect("write");
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    let elapsed = start.elapsed();

    println!(
        "8 submitters x 100 writes = 800 files committed in {elapsed:?} ({:.0} writes/sec)",
        800.0 / elapsed.as_secs_f64()
    );

    // Verify everything landed.
    let count = std::fs::read_dir(&dir)?.count();
    assert_eq!(count, 800, "all 800 files must be present");
    println!("durable: {count}/800 files verified on disk");

    std::fs::remove_dir_all(&dir).expect("cleanup");
    Ok(())
}
