//! # `Method::Direct` — bypass the OS page cache
//!
//! `Method::Direct` issues IO with the OS page cache disabled:
//! - Linux: `O_DIRECT` + `io_uring` (when available, kernel ≥ 5.1)
//!   or `O_DIRECT` + `pwrite` + `fdatasync` fallback.
//! - macOS: `fcntl(F_NOCACHE, 1)` after open + `F_FULLFSYNC`.
//! - Windows: `CreateFileW` with
//!   `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`.
//!
//! Buffer + offset + length alignment to the platform's logical
//! sector size (usually 512 or 4096) is handled internally; callers
//! pass arbitrary byte slices.
//!
//! Use `Direct` when:
//! - The workload is durability-critical and the application owns
//!   its own cache (e.g. a storage engine with its own page cache).
//! - On Linux + NVMe + io_uring, this is the fastest path.
//!
//! `Direct` falls back to `Data` then `Sync` on filesystems that
//! reject direct IO (tmpfs, some FUSE mounts). The fallback is
//! observable via `Handle::active_method()`.
//!
//! Run: `cargo run --example 05_method_direct`

use fsys::{builder, Method};

fn main() -> fsys::Result<()> {
    let fs = builder().method(Method::Direct).build()?;
    let path = std::env::temp_dir().join("fsys_example_direct.bin");

    // Arbitrary unaligned size — the buffer pool handles alignment.
    let payload = vec![0xA5u8; 8192 + 73];
    fs.write(&path, &payload)?;

    println!("requested method: Direct");
    println!("active method:    {}", fs.active_method());
    println!("primitive:        {}", fs.active_durability_primitive());
    println!("sector size:      {} bytes", fs.sector_size());

    let _ = std::fs::remove_file(&path);
    Ok(())
}
