//! # `Method::Auto` — hardware-aware automatic selection
//!
//! `Method::Auto` is the default. It probes the drive type and IO
//! primitive availability once at handle construction and picks the
//! fastest method that is safe on the current hardware and OS.
//!
//! Resolution ladder (abridged — full matrix in `Method::Auto`'s
//! rustdoc):
//!
//! | Condition                           | Resolves to |
//! |-------------------------------------|-------------|
//! | Linux + io_uring + NVMe             | `Direct`    |
//! | Linux + NVMe without io_uring       | `Data`      |
//! | Linux + SSD                         | `Data`      |
//! | Linux + HDD or unknown              | `Sync`      |
//! | macOS + NVMe                        | `Direct`    |
//! | macOS + non-NVMe SSD or unknown     | `Sync`      |
//! | Windows + NVMe                      | `Direct`    |
//! | Windows + SSD                       | `Direct`    |
//! | Windows + HDD or unknown            | `Sync`      |
//! | Hardware probe failed entirely      | `Sync` (universal safety) |
//!
//! `Auto` is the right pick unless you have a specific reason to
//! override. Even on `Auto`, the resolved method is observable.
//!
//! Run: `cargo run --example 07_method_auto`

use fsys::builder;

fn main() -> fsys::Result<()> {
    // `builder()` defaults to Method::Auto already.
    let fs = builder().build()?;
    let path = std::env::temp_dir().join("fsys_example_auto.txt");

    fs.write(&path, b"resolved by Auto")?;

    println!("Auto resolved to: {}", fs.active_method());
    println!("Primitive in use: {}", fs.active_durability_primitive());

    // Inspect the hardware probe that drove the decision.
    let hw = fsys::hardware::info();
    println!("\nHardware probe:");
    println!("  drive kind:        {:?}", hw.drive.kind);
    println!("  io_uring present:  {}", hw.io_primitives.io_uring);
    println!("  NVMe passthrough:  {}", hw.io_primitives.nvme_passthrough);

    let _ = std::fs::remove_file(&path);
    Ok(())
}
