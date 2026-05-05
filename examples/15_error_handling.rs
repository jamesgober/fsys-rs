//! # Error handling — match on `Error::code()` for stable log-grep
//!
//! Every `fsys::Error` variant has a stable `FS-NNNNN` code
//! returned by `Error::code()`. The codes are part of the API
//! contract; the `Display` formatting is not. For programmatic
//! handling and for log-grep, match on the code rather than the
//! variant name (resilient to `#[non_exhaustive]` future
//! additions) or the display string (which may be tightened
//! without a breaking change).
//!
//! Run: `cargo run --example 15_error_handling`

use fsys::{builder, Error};

fn main() {
    if let Err(e) = builder().build() {
        eprintln!("[{}] failed to build handle: {}", e.code(), e);
        std::process::exit(1);
    }

    // Trigger a path-escape error to demonstrate the code.
    let root = std::env::temp_dir().join("fsys_example_err_root");
    std::fs::create_dir_all(&root).ok();
    let scoped = builder().root(&root).build().unwrap();

    match scoped.write("../escaped.txt", b"x") {
        Ok(()) => println!("(unexpected: escape succeeded)"),
        Err(e) => {
            // Log-friendly format — code first, then display.
            eprintln!("[{}] write failed: {}", e.code(), e);

            // Programmatic handling — match on the code, not on the
            // variant. The codes are stable across versions.
            match e.code() {
                "FS-00002" => {
                    // Path-escape / invalid-path family.
                    eprintln!("  -> caller bug: path escapes the handle root");
                }
                "FS-00001" => {
                    // Underlying io::Error.
                    eprintln!("  -> system IO failure");
                }
                _ => {
                    eprintln!("  -> unexpected error category");
                }
            }

            // For when you DO want variant-level matching, use
            // `non_exhaustive`-aware patterns.
            if matches!(e, Error::InvalidPath { .. }) {
                eprintln!("  -> variant: Error::InvalidPath");
            }
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}
