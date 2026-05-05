//! # `quick` — one-shot file IO without holding a handle
//!
//! When you have a single file op and don't want to manage a `Handle`,
//! the `fsys::quick` module is the right tool: each call constructs a
//! default-method handle, runs the op, and drops the handle.
//!
//! For programs that do *more* than one IO op, prefer building a
//! single long-lived handle (see `02_handle_basics.rs`) — that avoids
//! re-paying the handle setup cost on every call.
//!
//! Run: `cargo run --example 01_quick_one_shot`

fn main() -> fsys::Result<()> {
    let path = std::env::temp_dir().join("fsys_example_quick.txt");

    // Atomic-replace write. The file is either entirely the old
    // payload (kill before rename) or entirely the new payload (kill
    // after rename) — never torn.
    fsys::quick::write(&path, b"hello from fsys::quick")?;

    // Full-file read.
    let bytes = fsys::quick::read(&path)?;
    println!("read back: {:?}", String::from_utf8_lossy(&bytes));

    // Cleanup.
    let _ = std::fs::remove_file(&path);
    Ok(())
}
