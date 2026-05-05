//! # `Method::Data` — data-only sync, faster than `Sync` on Linux
//!
//! `Method::Data` uses `fdatasync(2)` on Linux, which skips the
//! inode metadata write when the file size has not changed (mtime
//! still updates correctly on recovery, since it lives in the data
//! flush). On Linux + small writes this is observably faster than
//! `Sync`.
//!
//! On macOS and Windows there is no `fdatasync` equivalent, so
//! `Method::Data` falls back to `Sync` (`F_FULLFSYNC` /
//! `FlushFileBuffers`). The fallback is observable via
//! `Handle::active_method()` — it will read `Sync` on those
//! platforms after the fallback.
//!
//! Use `Data` when:
//! - You're on Linux.
//! - Your workload writes small payloads to existing files (so the
//!   file-size doesn't change and the fdatasync optimisation
//!   actually applies).
//!
//! Run: `cargo run --example 04_method_data`

use fsys::{builder, Method};

fn main() -> fsys::Result<()> {
    let fs = builder().method(Method::Data).build()?;
    let path = std::env::temp_dir().join("fsys_example_data.txt");

    // First write — file is created, size changes; fdatasync may
    // still flush the metadata page.
    fs.write(&path, b"first write")?;

    // Subsequent in-place updates of the same size benefit fully
    // from fdatasync's metadata-skip on Linux.
    fs.write(&path, b"second write")?;

    println!("requested method: Data");
    println!("active method:    {}", fs.active_method());
    println!("primitive:        {}", fs.active_durability_primitive());

    let _ = std::fs::remove_file(&path);
    Ok(())
}
