//! # `Method::Mmap` — memory-mapped IO with `msync` for durability
//!
//! `Method::Mmap` reads through a memory mapping and writes flow
//! through the mapping with explicit `msync(MS_SYNC)` (Linux/macOS)
//! or `FlushViewOfFile` (Windows) for durability.
//!
//! It is most useful for **read-heavy random-access** workloads,
//! where the kernel's page-cache prefetching outperforms repeated
//! `pread` syscalls. For write-heavy workloads, prefer `Direct` or
//! `Data` — `Mmap` writes still require a sync call for durability.
//!
//! `Mmap` falls back to `Sync` for files smaller than the page size,
//! special files (sockets, pipes, FIFOs), and filesystems that reject
//! `mmap`. The fallback is observable via `Handle::active_method()`.
//!
//! Run: `cargo run --example 06_method_mmap`

use fsys::{builder, Method};

fn main() -> fsys::Result<()> {
    let fs = builder().method(Method::Mmap).build()?;
    let dir = std::env::temp_dir();
    let path = dir.join("fsys_example_mmap.bin");

    // Write some payload (Mmap covers writes too, with msync).
    let payload = vec![0xCDu8; 64 * 1024];
    fs.write(&path, &payload)?;

    // The read path — for read-heavy random access, this is where
    // mmap shines.
    let bytes = fs.read(&path)?;
    println!("read {} bytes via Mmap", bytes.len());
    println!("active method:    {}", fs.active_method());
    println!("primitive:        {}", fs.active_durability_primitive());

    let _ = std::fs::remove_file(&path);
    Ok(())
}
