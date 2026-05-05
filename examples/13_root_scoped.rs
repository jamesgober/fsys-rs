//! # Root-scoped handle — keep IO inside a known subtree
//!
//! `Builder::root(path)` binds a handle to a base directory.
//! Every public method that takes a path resolves it against this
//! root and rejects paths that escape. This is the right tool for
//! sandboxing: an HTTP server that writes uploads only under
//! `/var/uploads`, a build tool that writes only under `target/`,
//! a test that writes only under a tempdir.
//!
//! Resolution rules:
//! - Relative paths are joined to the root.
//! - Absolute paths are accepted only if they're already under the
//!   root.
//! - Paths containing `..` that resolve outside the root are rejected
//!   with `Error::InvalidPath`.
//!
//! Run: `cargo run --example 13_root_scoped`

use fsys::builder;

fn main() -> fsys::Result<()> {
    // Set up a sandbox directory.
    let root = std::env::temp_dir().join("fsys_example_sandbox");
    std::fs::create_dir_all(&root).ok();

    // Bind the handle to the sandbox.
    let fs = builder().root(&root).build()?;

    // Relative path: gets joined to the root.
    fs.write("note.txt", b"inside the sandbox")?;
    println!(
        "wrote 'note.txt' (resolved to {})",
        root.join("note.txt").display()
    );

    // Path-escape attempt: rejected with InvalidPath.
    let escape_attempt = fs.write("../escape.txt", b"trying to escape");
    match escape_attempt {
        Err(fsys::Error::InvalidPath { .. }) => {
            println!("correctly rejected path-escape attempt");
        }
        Err(e) => return Err(e),
        Ok(()) => panic!("path-escape was NOT rejected — security bug!"),
    }

    // Cleanup.
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}
