//! # `Handle::punch_hole` / `write_zeros` — WAL trim primitive
//!
//! 0.9.5 added cross-platform sparse-file primitives:
//!
//! - `punch_hole(path, offset, len)` — deallocates `[offset, offset+len)`,
//!   leaving a sparse hole. Reads of the hole return zeros; the file's
//!   logical size is unchanged.
//! - `write_zeros(path, offset, len)` — writes zeros to the range,
//!   keeping the file's allocation.
//!
//! Per-platform mapping:
//!
//! | Platform | Primitive |
//! |---|---|
//! | Linux | `fallocate(FALLOC_FL_PUNCH_HOLE \| FALLOC_FL_KEEP_SIZE)` |
//! | macOS | `fcntl(F_PUNCHHOLE)` |
//! | Windows | `FSCTL_SET_ZERO_DATA` |
//!
//! All three are kernel-atomic.
//!
//! ## When to use this pattern
//!
//! - **Database WAL trim** — after a checkpoint flushes records
//!   [start_lsn, checkpoint_lsn), punch a hole in that range to
//!   give the storage back to the filesystem without touching the
//!   page cache or rewriting the WAL.
//! - **Log compaction** — sparse-out the deallocated middle of a
//!   log segment after replay completes.
//! - **Sparse file production** — reserve a 1 GiB logical file but
//!   only allocate the regions actively written to.
//!
//! ## When NOT to use this pattern
//!
//! - **Filesystems without sparse support** — FAT32, exFAT, some
//!   network mounts. The call may succeed with no effect, or may
//!   return an error. Test on your target filesystem before
//!   depending on the space-reclaim behaviour.
//! - **Database engines whose WAL format is sequential-scan-only** —
//!   sparse-out regions can confuse readers that don't expect zero
//!   ranges. Use the read-side sparse detection (`SEEK_HOLE` /
//!   `SEEK_DATA` on POSIX) if you need to enumerate live regions
//!   after a punch.
//!
//! Run: `cargo run --example 21_punch_hole_wal_trim`

fn main() -> fsys::Result<()> {
    let path = std::env::temp_dir().join("fsys_example_punch_hole.dat");
    let _ = std::fs::remove_file(&path);

    let fs = fsys::builder().build()?;

    // Build a 1 MiB file of 'A' bytes — represents a packed WAL
    // segment full of records that have all been checkpointed.
    let payload = vec![b'A'; 1024 * 1024];
    fs.write(&path, &payload)?;
    let size_before = std::fs::metadata(&path)?.len();
    println!("WAL segment built: {size_before} bytes");

    // Punch a hole in the middle 512 KiB — represents trimming the
    // checkpointed records back to the filesystem.
    let trim_offset = 256 * 1024_u64;
    let trim_len = 512 * 1024_u64;
    fs.punch_hole(&path, trim_offset, trim_len)?;
    println!(
        "punched hole [offset={trim_offset}, len={trim_len}] = 512 KiB returned to filesystem"
    );

    // Logical size is unchanged — the file still presents as 1 MiB.
    let size_after = std::fs::metadata(&path)?.len();
    assert_eq!(size_after, size_before, "punch_hole preserves logical size");

    // Reads of the hole return zeros.
    let after_punch = std::fs::read(&path)?;
    let hole_region = &after_punch[trim_offset as usize..(trim_offset + trim_len) as usize];
    let is_all_zero = hole_region.iter().all(|&b| b == 0);
    println!("hole region reads as zeros: {is_all_zero}");

    // Surrounding bytes are still the original 'A' content.
    assert_eq!(after_punch[0], b'A', "head of file untouched");
    assert_eq!(after_punch[after_punch.len() - 1], b'A', "tail untouched");
    println!("non-hole regions preserved");

    // write_zeros is the variant that zeros without deallocating —
    // useful when the engine wants zero-content but wants to keep
    // the allocation for predictable future writes at this offset.
    fs.write_zeros(&path, 0, 256 * 1024)?;
    println!("first 256 KiB zeroed via write_zeros (kept allocation)");

    let _ = std::fs::remove_file(&path);
    Ok(())
}
