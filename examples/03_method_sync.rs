//! # `Method::Sync` — strongest durability, available everywhere
//!
//! `Method::Sync` calls the platform's strongest durability primitive:
//! - Linux: `fsync(2)`
//! - macOS: `fcntl(F_FULLFSYNC, 0)`  *(NOT regular `fsync` — that does
//!   not guarantee media durability on Apple's storage stack)*
//! - Windows: `FlushFileBuffers`
//!
//! Use `Sync` when you cannot tolerate data loss across power-loss
//! events and the workload can absorb the latency cost. It is the
//! safest default and is always supported.
//!
//! Naming caveat: `Sync` here refers to the `fsync(2)` family of
//! durability primitives — **not** "synchronous IO" as opposed to
//! async. All methods work from both the sync and async APIs.
//!
//! Run: `cargo run --example 03_method_sync`

use fsys::{builder, Method};

fn main() -> fsys::Result<()> {
    let fs = builder().method(Method::Sync).build()?;
    let path = std::env::temp_dir().join("fsys_example_sync.txt");

    fs.write(&path, b"durable via fsync/F_FULLFSYNC/FlushFileBuffers")?;
    println!("active method:        {}", fs.active_method());
    println!("durability primitive: {}", fs.active_durability_primitive());

    let _ = std::fs::remove_file(&path);
    Ok(())
}
