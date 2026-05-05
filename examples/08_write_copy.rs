//! # `write_copy` — atomic replace preserving target metadata
//!
//! `Handle::write_copy(path, data)` is **not** a file-to-file copy
//! operation (no source argument). It is a write that **copies the
//! existing target's metadata onto the new payload before swapping
//! it in** — mode, ACLs, owner/group, timestamps. If you want a real
//! file-to-file copy, use `std::fs::copy`.
//!
//! Use case: you're updating a config file at `/etc/foo.conf` that
//! has been carefully chmod'd to `0644` and chown'd to `root:wheel`.
//! `write_copy` replaces the contents while preserving those bits.
//!
//! Implemented as **atomic swap only** — the file at `path` is
//! either entirely the old payload (kill before rename) or entirely
//! the new payload (kill after rename). The metadata-preservation
//! step happens on the staging file before the rename, so a crash
//! mid-`chmod` leaves the target file unchanged.
//!
//! Run: `cargo run --example 08_write_copy`

use fsys::builder;

fn main() -> fsys::Result<()> {
    let fs = builder().build()?;
    let path = std::env::temp_dir().join("fsys_example_write_copy.conf");

    // Set up: write the file once with default permissions.
    fs.write(&path, b"original payload")?;

    let before = fs.meta(&path)?;
    println!(
        "before: size={} bytes, mtime={:?}",
        before.size, before.modified
    );

    // Replace the contents while preserving mode/ACLs/ownership.
    fs.write_copy(&path, b"replacement payload - same mode bits as before")?;

    let after = fs.meta(&path)?;
    println!(
        "after:  size={} bytes, mtime={:?}",
        after.size, after.modified
    );
    println!(
        "readonly preserved: {} (expect: false on default file)",
        after.readonly
    );

    let _ = std::fs::remove_file(&path);
    Ok(())
}
