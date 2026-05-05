//! # Fluent batch builder — `Handle::batch().write(...).commit()`
//!
//! For very large or dynamic batches where building a slice of
//! `(path, data)` tuples up-front is awkward, the chainable
//! `Batch` builder accumulates ops one at a time:
//!
//! ```ignore
//! handle.batch()
//!       .write("a.txt", b"alpha")
//!       .write("b.txt", b"beta")
//!       .delete("stale.txt")
//!       .copy("src.txt", "dst.txt")
//!       .commit()?;
//! ```
//!
//! The builder allocates per `.write()` / `.delete()` / `.copy()`
//! call (paced allocation across the build loop, not one big burst
//! at commit). For a 10K-op batch you pay 10K small allocations
//! spread across the build, then one submission.
//!
//! Run: `cargo run --example 10_batch_builder`

use fsys::builder;

fn main() -> fsys::Result<()> {
    let fs = builder().build()?;
    let dir = std::env::temp_dir();

    // Pre-create a "stale" file so the .delete() in the chain
    // demonstrates removing existing content.
    fs.write(dir.join("fsys_stale.txt"), b"about to be deleted")?;

    let mut batch = fs.batch();
    batch
        .write(dir.join("fsys_chain_a.txt"), b"alpha")
        .write(dir.join("fsys_chain_b.txt"), b"beta")
        .write(dir.join("fsys_chain_c.txt"), b"gamma")
        .delete(dir.join("fsys_stale.txt"));
    println!("queued {} ops", batch.len());

    if let Err(e) = batch.commit() {
        return Err(*e.into_inner());
    }
    println!("commit succeeded");

    // Cleanup.
    for n in ["a", "b", "c"] {
        let _ = std::fs::remove_file(dir.join(format!("fsys_chain_{n}.txt")));
    }
    Ok(())
}
