//! # `Handle::copy` — reflink-aware copy on APFS / ReFS
//!
//! 0.9.6 added reflink fast-path to `Handle::copy`. On supported
//! filesystems, the platform's copy-on-write clone syscall makes a
//! multi-GiB file copy instantaneous (microseconds, regardless of
//! file size):
//!
//! - **macOS APFS** → `clonefile(2)` — the entire APFS extent map
//!   is shared between source and destination; future writes to
//!   either file allocate new blocks lazily (the CoW part).
//! - **Windows ReFS** → `FSCTL_DUPLICATE_EXTENTS_TO_FILE` — same
//!   extent-sharing semantics. Requires both source and destination
//!   on the same ReFS volume.
//! - **Linux btrfs / XFS reflinks** — not yet wired in fsys (would
//!   use `ioctl_ficlone` / `copy_file_range`); falls back to
//!   `std::fs::copy`.
//! - **Everything else** — falls back to `std::fs::copy`, which is
//!   a byte-by-byte stream copy. Scales linearly with file size.
//!
//! The fallback is **silent**: `Handle::copy` returns `Ok(bytes)`
//! either way. Inspect wall-clock time on large files to confirm
//! the reflink path engaged.
//!
//! ## When to use this pattern
//!
//! - **Database checkpoint clones.** Take a logical "snapshot" of
//!   a multi-GiB SSTable by reflink-copying it; writers continue on
//!   the original while readers see the snapshot.
//! - **Container layering.** Reflink-clone a base image instead of
//!   bytewise copy.
//! - **Backup tooling.** Initial backup via reflink is free; only
//!   subsequent divergence costs allocations.
//!
//! ## When NOT to use this pattern
//!
//! - Cross-volume copies — the kernel always falls back to bytewise.
//! - NTFS / HFS+ / ext4-without-reflink targets — same.
//! - When you actually want a divergent copy immediately — reflink
//!   shares extents, so the first write to either side allocates;
//!   bytewise copy front-loads all the allocation work.
//!
//! ## Verifying the fast path engaged
//!
//! `fsys` doesn't surface a "was-reflink" flag yet; inspect wall-clock
//! on a large file. Reflink: < 10 ms regardless of size. Bytewise:
//! scales with file size.
//!
//! Run: `cargo run --example 29_reflink_aware_copy`

fn main() -> fsys::Result<()> {
    let dir = std::env::temp_dir().join("fsys_example_reflink");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");

    let src = dir.join("source.dat");
    let dst = dir.join("clone.dat");

    let fs = fsys::builder().build()?;

    // Build a 16 MiB source — small enough for the example, large
    // enough that the reflink vs bytewise difference is observable
    // on platforms that support reflink.
    let size_mib = 16;
    let payload = vec![0xCDu8; size_mib * 1024 * 1024];
    fs.write(&src, &payload)?;
    println!("source built: {size_mib} MiB at {}", src.display());

    // Copy. On APFS / ReFS this engages clonefile / FSCTL; elsewhere
    // it falls back to std::fs::copy.
    let start = std::time::Instant::now();
    let bytes = fs.copy(&src, &dst)?;
    let elapsed = start.elapsed();

    println!();
    println!("copied {bytes} bytes in {elapsed:?}");
    let throughput_mibs = (size_mib as f64) / elapsed.as_secs_f64();
    if elapsed.as_millis() < 50 {
        println!("  → likely reflink (APFS/ReFS) — {throughput_mibs:.0} MiB/s effective");
    } else {
        println!("  → likely bytewise fallback — {throughput_mibs:.0} MiB/s actual");
    }

    // Verify: byte-for-byte equality.
    let src_data = std::fs::read(&src)?;
    let dst_data = std::fs::read(&dst)?;
    assert_eq!(src_data, dst_data, "copy must produce byte-identical file");
    println!("verified: source and clone are byte-identical");

    println!();
    println!("platform-specific notes:");
    #[cfg(target_os = "macos")]
    println!("  this platform: macOS — Handle::copy uses clonefile(2) on APFS");
    #[cfg(target_os = "windows")]
    println!("  this platform: Windows — Handle::copy uses FSCTL_DUPLICATE_EXTENTS_TO_FILE on ReFS");
    #[cfg(target_os = "linux")]
    println!("  this platform: Linux — Handle::copy falls back to std::fs::copy (btrfs/XFS reflink not wired)");

    std::fs::remove_dir_all(&dir).expect("cleanup");
    Ok(())
}
