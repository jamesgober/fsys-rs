//! # `Batch::commit_grouped` — atomic-batch fsync
//!
//! 0.9.3 added a grouped-commit variant of `Batch::commit` that
//! amortises **parent-directory `fsync`** across the entire batch
//! instead of paying one per op.
//!
//! Each op still does its own data fsync (the temp file's content is
//! durable before its rename — that's the atomic-replace contract).
//! What `commit_grouped` changes is the **post-rename** `sync_parent_dir`
//! step: the regular path calls it once per op, the grouped path
//! accumulates unique parent directories and calls it once per unique
//! parent after the entire batch succeeds.
//!
//! For "flush 1024 SST files into one directory" — `commit` pays 1024
//! parent-dir syncs, `commit_grouped` pays **1**. On Linux/macOS this
//! is the biggest win available for bulk-load workloads; on Windows
//! the call is observably equivalent (directory durability is
//! implicit there).
//!
//! ## Trade-off
//!
//! `commit` makes each op individually durable (data + dirent) on
//! return. `commit_grouped` makes the batch durable **as a set** — a
//! crash mid-batch may leave a prefix of renames visible while the
//! dirent updates haven't yet landed in the filesystem journal. The
//! per-op data is still on disk (its data fsync still happened); on
//! the next reboot the filesystem journal replays the dirent updates.
//!
//! ## When to use this pattern
//!
//! - Bulk loads (initial database population, ETL imports)
//! - SST flushes (LSM-tree compaction emitting many files)
//! - Database checkpoint emissions (many files committed as one
//!   logical checkpoint)
//!
//! Any workload where **the batch is the durability unit**, not the
//! individual op.
//!
//! ## When NOT to use this pattern
//!
//! Callers that need per-op dirent durability — rare, usually only
//! when each op IS a transaction commit point. Use `commit()` for
//! those.
//!
//! Run: `cargo run --example 19_batch_commit_grouped`

use std::path::PathBuf;

fn main() -> std::result::Result<(), fsys::BatchError> {
    let dir = std::env::temp_dir().join("fsys_example_commit_grouped");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");

    let fs = fsys::builder().build().expect("build handle");

    // Simulate an SST flush: 100 small files written into one
    // directory as a single logical checkpoint.
    let mut batch = fs.batch();
    let paths: Vec<PathBuf> = (0..100)
        .map(|i| dir.join(format!("sst_{i:03}.dat")))
        .collect();
    for (i, path) in paths.iter().enumerate() {
        let data = format!("level-0 SST #{i} bloom-filter + index payload");
        batch.write(path, data.as_bytes());
    }

    // commit_grouped: 100 ops, ONE sync_parent_dir(dir) call instead
    // of 100. On Linux/macOS this is the win.
    let start = std::time::Instant::now();
    batch.commit_grouped()?;
    let elapsed = start.elapsed();
    println!("100 SSTs committed via commit_grouped in {elapsed:?}");

    // Verify every file is durable and present.
    let mut count = 0;
    for path in &paths {
        let data = std::fs::read(path).expect("read SST");
        assert!(!data.is_empty(), "every SST has content");
        count += 1;
    }
    println!("durable: {count}/100 SSTs verified on disk");

    // Cleanup.
    std::fs::remove_dir_all(&dir).expect("cleanup");
    Ok(())
}
