//! # Capability cache lifecycle (1.1.0)
//!
//! Demonstrates the three public capability-cache entry points:
//!
//! - [`fsys::capability::capabilities`] — cached, sub-millisecond.
//! - [`fsys::capability::probe_capabilities_fresh`] — forces a
//!   re-probe and rewrites the cache file.
//! - [`fsys::capability::invalidate_capability_cache`] — deletes
//!   the cache file so the next process re-probes.
//!
//! The example also shows the timing delta between a cached
//! lookup and a fresh probe so consumers see why caching matters.
//!
//! Run: `cargo run --example 31_capability_cache_lifecycle`

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // First call to `capabilities()` will read the cache file (or
    // probe + write it if missing). Subsequent calls hit the
    // process-wide `OnceLock`.
    let t = Instant::now();
    let first = fsys::capability::capabilities();
    let first_call = t.elapsed();

    let t = Instant::now();
    let _second = fsys::capability::capabilities();
    let second_call = t.elapsed();

    println!("capabilities() — first call:  {:?}", first_call);
    println!("capabilities() — second call: {:?}", second_call);
    println!("  fsys version: {}", first.fsys_version);
    println!("  os target:    {}", first.os_target);
    println!("  spdk eligible: {}", first.spdk_eligible);
    println!();

    // Forced fresh probe — ignores the cache file, writes a new
    // one on success. Note this does *not* update the process-wide
    // `OnceLock` — that is only re-populated on a fresh process.
    let t = Instant::now();
    let fresh = fsys::capability::probe_capabilities_fresh();
    let fresh_probe = t.elapsed();

    println!("probe_capabilities_fresh(): {:?}", fresh_probe);
    println!("  schema version: {}", fresh.schema_version);
    println!();

    // Cache file location (overridable via FSYS_CACHE_DIR).
    match fsys::capability::cache::cache_file_path() {
        Some(p) => println!("Cache file: {}", p.display()),
        None => println!("Cache file: <no candidate directory>"),
    }
    println!();

    // Invalidation deletes the cache file. The next process start
    // will re-probe and rewrite. Within this process the cached
    // `OnceLock` remains valid for backwards-compatibility — the
    // contract is "delete the file"; the cached in-memory snapshot
    // outlives the file.
    fsys::capability::invalidate_capability_cache()?;
    println!("invalidate_capability_cache() — cache file deleted");

    Ok(())
}
