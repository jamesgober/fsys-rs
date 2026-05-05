//! # `Handle` basics — the long-lived IO handle pattern
//!
//! For any program that does more than a single IO op, build a
//! `Handle` once and reuse it. The handle owns the resolved method,
//! the buffer pool, the pipeline dispatcher, and the io_uring slot
//! (Linux). Constructing it is non-trivial; *using* it is cheap.
//!
//! `Handle` is `Send + Sync + Clone` — clone freely across threads;
//! all clones share the same underlying resources via `Arc`.
//!
//! Run: `cargo run --example 02_handle_basics`

use fsys::builder;

fn main() -> fsys::Result<()> {
    // One handle, default settings. `builder().build()` resolves
    // `Method::Auto` against the hardware probe and picks the fastest
    // safe method for this machine.
    let fs = builder().build()?;

    // Observe what `Auto` actually picked.
    println!("active method: {}", fs.active_method());
    println!("durability primitive: {}", fs.active_durability_primitive());

    // Use the handle for everything you need.
    let dir = std::env::temp_dir();
    let p1 = dir.join("fsys_example_basics_1.txt");
    let p2 = dir.join("fsys_example_basics_2.txt");

    fs.write(&p1, b"first")?;
    fs.write(&p2, b"second")?;

    println!("p1 exists: {}", fs.exists(&p1)?);
    println!("p2 size:   {} bytes", fs.meta(&p2)?.size);

    fs.delete(&p1)?;
    fs.delete(&p2)?;
    Ok(())
}
