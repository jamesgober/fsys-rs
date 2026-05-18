//! # Capability probe — what does this host support? (1.1.0)
//!
//! [`fsys::capability::capabilities`] returns a cached snapshot of
//! everything 1.1.0 backend selection depends on: io_uring kernel
//! features, NVMe passthrough availability, Direct-IO support, PLP
//! detection, and SPDK eligibility (with specific skip reasons when
//! preconditions fail).
//!
//! The first call to `capabilities()` runs the full probe
//! (50-200 ms on a typical host). Subsequent calls read from a
//! process-wide [`OnceLock`](std::sync::OnceLock) and complete in
//! well under a millisecond — safe to call from a per-second
//! health-check loop.
//!
//! Run: `cargo run --example 30_capability_probe`

fn main() {
    let caps = fsys::capability::capabilities();

    println!("fsys capability snapshot");
    println!("════════════════════════");
    println!("  schema version:   {}", caps.schema_version);
    println!("  fsys version:     {}", caps.fsys_version);
    println!("  kernel version:   {}", caps.kernel_version);
    println!("  os target:        {}", caps.os_target);
    println!("  probed at:        unix={}", caps.probed_at_unix_secs);
    println!();

    println!("Kernel IO primitives");
    println!("────────────────────");
    println!("  io_uring:          {}", caps.io_uring);
    if !caps.io_uring_features.is_empty() {
        let names: Vec<&str> = caps.io_uring_features.iter().map(|f| f.as_str()).collect();
        println!("  io_uring features: {}", names.join(", "));
    }
    println!("  nvme passthrough:  {}", caps.nvme_passthrough);
    println!("  direct io:         {}", caps.direct_io);
    println!("  plp detected:      {}", caps.plp_detected);
    println!();

    println!("SPDK eligibility");
    println!("────────────────");
    println!("  eligible:          {}", caps.spdk_eligible);
    if !caps.spdk_skip_reasons.is_empty() {
        println!("  skip reasons:");
        for r in &caps.spdk_skip_reasons {
            println!("    • {r}");
        }
    }
    if !caps.spdk_eligible_devices.is_empty() {
        println!("  eligible devices:");
        for d in &caps.spdk_eligible_devices {
            println!("    • {d}");
        }
    }
    println!();

    println!("Hardware summary");
    println!("────────────────");
    println!("  drive type:        {}", caps.hardware.drive_type);
    println!(
        "  optimal block:     {} bytes",
        caps.hardware.optimal_block_size
    );
    println!("  queue depth:       {}", caps.hardware.queue_depth);
    println!(
        "  sector logical:    {} bytes",
        caps.hardware.sector_size_logical
    );
    println!(
        "  sector physical:   {} bytes",
        caps.hardware.sector_size_physical
    );
    println!();

    if let Some(path) = fsys::capability::cache::cache_file_path() {
        println!("Cache file: {}", path.display());
        println!("(set FSYS_REPROBE=1 to force a re-probe on next start)");
    } else {
        println!("Cache file: <no candidate directory; caching disabled>");
    }
}
