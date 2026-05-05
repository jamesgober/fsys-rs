//! # Tuning Direct IO — buffer pool + io_uring queue depth
//!
//! `Method::Direct` exposes three knobs that a workload-specific
//! tuning pass can move:
//!
//! - `buffer_pool_count(n)` — number of aligned buffers in the
//!   per-handle pool (default 64). Idle handles cost zero buffer
//!   memory; the pool is allocated lazily on the first Direct op.
//!   Larger pool = less allocation pressure on bursty workloads,
//!   higher per-handle resident memory.
//! - `buffer_pool_block_size(bytes)` — per-buffer size in bytes
//!   (default 4096). Direct workloads with payloads larger than the
//!   default benefit from 64 KiB or 1 MiB blocks (fewer leases per
//!   op).
//! - `io_uring_queue_depth(depth)` — Linux io_uring SQ depth
//!   (default 128). Higher depth helps when the workload has many
//!   in-flight ops; lower depth reduces kernel memory.
//!
//! These are tuning knobs, not contracts — defaults are chosen for
//! "good enough on most workloads." Move them only after
//! benchmarking against the workload you actually have. See
//! `docs/PERFORMANCE.md` for tuning guidance.
//!
//! Run: `cargo run --example 16_tuning_direct`

use fsys::{builder, Method};

fn main() -> fsys::Result<()> {
    // Tuned for a workload with large (1 MiB) payloads and many
    // concurrent in-flight ops. Larger blocks mean fewer pool leases
    // per op; bigger queue depth means more parallelism on the
    // io_uring submission side (Linux only — Windows/macOS ignore
    // the queue depth knob).
    let fs = builder()
        .method(Method::Direct)
        .buffer_pool_count(32)            // fewer, larger buffers
        .buffer_pool_block_size(1 << 20)  // 1 MiB per buffer
        .io_uring_queue_depth(256)         // Linux SQ depth
        .build()?;

    let path = std::env::temp_dir().join("fsys_example_tuned_direct.bin");

    // 1 MiB payload — fits in a single buffer-pool lease.
    let payload = vec![0xFFu8; 1 << 20];
    fs.write(&path, &payload)?;

    println!("active method:      {}", fs.active_method());
    println!("primitive:          {}", fs.active_durability_primitive());
    println!("sector size:        {} bytes", fs.sector_size());
    println!("payload size:       {} bytes", payload.len());

    let _ = std::fs::remove_file(&path);
    Ok(())
}
