//! # Async batch — `write_batch_async`
//!
//! Async batch ops route through the same per-handle group-lane
//! dispatcher as sync batches (per locked decision D-5 in 0.6.0),
//! but the response wakes via a `tokio::sync::oneshot` instead of
//! a crossbeam channel. The dispatcher matches exhaustively on the
//! `BatchResponse` enum so both paths share serialisation logic.
//!
//! Build: `cargo run --example 12_async_batch --features async`

use fsys::builder;
use std::sync::Arc;

#[tokio::main]
async fn main() -> fsys::Result<()> {
    let fs = Arc::new(builder().build()?);
    let dir = std::env::temp_dir();

    // Build the batch as Vec<(PathBuf, Vec<u8>)> — the async API
    // takes owned values because the future may outlive any
    // borrow of the caller's data.
    let batch: Vec<(std::path::PathBuf, Vec<u8>)> = (0..6)
        .map(|n| {
            (
                dir.join(format!("fsys_async_batch_{n}.txt")),
                format!("async file {n}").into_bytes(),
            )
        })
        .collect();

    let count = batch.len();
    if let Err(e) = fs.clone().write_batch_async(batch.clone()).await {
        eprintln!(
            "async batch failed at op {}, completed={}",
            e.failed_at(),
            e.completed()
        );
        return Err(*e.into_inner());
    }
    println!("submitted async batch of {count} writes");

    // Clean up.
    let paths: Vec<std::path::PathBuf> = batch.into_iter().map(|(p, _)| p).collect();
    if let Err(e) = fs.clone().delete_batch_async(paths).await {
        return Err(*e.into_inner());
    }

    Ok(())
}
