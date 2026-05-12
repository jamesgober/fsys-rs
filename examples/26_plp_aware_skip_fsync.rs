//! # `Handle::is_plp_protected()` / `plp_status()` — PLP-aware fsync skip
//!
//! 0.9.2 added Power-Loss-Protection detection. Enterprise NVMe
//! drives have capacitors that back the controller's volatile cache
//! through a power loss; writes acknowledged into the cache survive
//! reboot. When PLP is confirmed, databases can safely **skip
//! per-commit fsync** for transaction throughput — a 3–10× win on
//! many workloads.
//!
//! The two accessors:
//!
//! - `Handle::is_plp_protected() -> bool` — simple yes/no. Returns
//!   `false` when PLP is not confirmed (covers both NotDetected and
//!   probe-failed cases).
//! - `Handle::plp_status() -> PlpStatus` — full ternary state:
//!   `Yes` (confirmed present), `No` (confirmed absent), `Unknown`
//!   (could not be determined). The `Unknown` state is non-fatal
//!   — fsys continues with conservative defaults — but load-bearing
//!   for safe-fsync-skip decisions.
//!
//! ## Safe fsync-skip contract
//!
//! Only skip fsync when:
//!
//! 1. **`plp_status() == PlpStatus::Yes`** — confirmed PLP.
//!    Treat `Unknown` and `No` as "PLP not present" for safety.
//! 2. **The drive's PLP capacitor is in service** — most enterprise
//!    drives have a SMART attribute reporting capacitor health
//!    (e.g. NVMe attribute 0x0B). Production code should check this
//!    periodically; this example doesn't.
//! 3. **The IO request is durable on power loss** — `O_DIRECT` /
//!    `Method::Direct` typically required. Buffered writes that
//!    haven't reached the device's cache are NOT covered by PLP.
//!
//! ## When to use this pattern
//!
//! Database transaction commits on enterprise NVMe. The fsync skip
//! is the load-bearing latency lever for high-throughput OLTP — see
//! the "Microsoft Group Commit" technique for analogous design.
//!
//! ## When NOT to use this pattern
//!
//! - Consumer NVMe (no PLP capacitor — your transactions WILL lose
//!   on power loss)
//! - Cross-platform deployments where you can't guarantee PLP on
//!   every target
//! - Workloads where commit-loss-on-crash is unacceptable regardless
//!   of "rarity" arguments
//!
//! Run: `cargo run --example 26_plp_aware_skip_fsync`

fn main() -> fsys::Result<()> {
    let fs = fsys::builder().method(fsys::Method::Direct).build()?;

    // Read the PLP probe result.
    let plp = fs.is_plp_protected();
    let plp_full = fs.plp_status();
    println!("PLP probe result:");
    println!("  is_plp_protected: {plp}");
    println!("  plp_status:       {plp_full:?}");

    // Production decision table: skip fsync only when Detected.
    let safe_to_skip_fsync = matches!(plp_full, fsys::hardware::PlpStatus::Yes);
    println!();
    println!("decision:");
    if safe_to_skip_fsync {
        println!("  ✓ PLP confirmed — application MAY skip per-commit fsync");
        println!("    (still required: confirm capacitor health via SMART)");
    } else {
        println!("  ✗ PLP not confirmed — application MUST keep per-commit fsync");
        println!("    (this is the safe default on consumer hardware)");
    }

    // Demonstrate the latency difference (using fs.write which always
    // syncs — the actual fsync-skip would happen at a layer above,
    // in the database's transaction commit path).
    let path = std::env::temp_dir().join("fsys_example_plp_aware.dat");
    let _ = std::fs::remove_file(&path);

    let payload = b"transaction commit record";
    let start = std::time::Instant::now();
    for _ in 0..10 {
        fs.write(&path, payload)?;
    }
    let elapsed = start.elapsed();
    println!();
    println!(
        "10 durable writes via fsys::Direct in {elapsed:?} ({:.1} ms each)",
        elapsed.as_secs_f64() * 1000.0 / 10.0
    );
    println!("on PLP-confirmed enterprise NVMe, the fsync-skip path would");
    println!("be ~3-10x faster than this baseline (cache-only durability)");

    let _ = std::fs::remove_file(&path);
    Ok(())
}
