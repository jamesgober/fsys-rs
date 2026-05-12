//! # Group-lane batch — `write_batch` / `delete_batch` / `copy_batch`
//!
//! When you have many files to write/delete/copy and durability
//! matters per file, the per-handle group-lane batch API amortises
//! syscall overhead across the batch. Submission is one call; the
//! per-handle dispatcher serialises the durability ops within its
//! lane.
//!
//! Compared to looping over `Handle::write` for each file, batch
//! submission:
//! - Pays one round-trip to the dispatcher instead of N.
//! - Lets the dispatcher coalesce sync calls when safe.
//! - Reports failure with the **failed_at index** plus the count
//!   that succeeded before it (`BatchError::completed`).
//!
//! Run: `cargo run --example 09_batch_slice`

use fsys::builder;

fn main() -> fsys::Result<()> {
    let fs = builder().build()?;
    let dir = std::env::temp_dir();

    // Build a slice of (path, data) tuples and submit it as one
    // batch. PathBufs live for the duration of the batch; the data
    // slices reference our own buffers.
    let p = |n: usize| dir.join(format!("fsys_example_batch_{n}.txt"));
    let paths: Vec<_> = (0..6).map(p).collect();
    let payloads: Vec<Vec<u8>> = (0..6)
        .map(|n| format!("file number {n}").into_bytes())
        .collect();

    let writes: Vec<(&std::path::PathBuf, &[u8])> = paths
        .iter()
        .zip(payloads.iter())
        .map(|(path, data)| (path, data.as_slice()))
        .collect();

    if let Err(e) = fs.write_batch(&writes) {
        // BatchError carries a failed_at index + completed count.
        eprintln!(
            "batch failed at op {}, completed={}",
            e.failed_at(),
            e.completed()
        );
        return Err(*e.into_inner());
    }
    println!("wrote {} files in one batch", writes.len());

    // Clean up via delete_batch for symmetry.
    if let Err(e) = fs.delete_batch(&paths) {
        return Err(*e.into_inner());
    }
    println!("deleted {} files in one batch", paths.len());

    Ok(())
}
