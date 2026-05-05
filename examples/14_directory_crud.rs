//! # Directory CRUD — mkdir_all, scan_all, find, count_all
//!
//! `fsys` separates the flat (non-recursive) and recursive
//! directory-walk methods into distinct names rather than taking a
//! `recursive: bool` parameter:
//!
//! | Flat (non-recursive) | Recursive          |
//! |---------------------|---------------------|
//! | `Handle::scan`      | `Handle::scan_all`  |
//! | `Handle::count`     | `Handle::count_all` |
//!
//! `Handle::find` is the exception — it takes a glob pattern, and
//! recursion is encoded in the pattern itself (`*` non-recursive,
//! `**` recursive).
//!
//! Run: `cargo run --example 14_directory_crud`

use fsys::builder;

fn main() -> fsys::Result<()> {
    let fs = builder().build()?;
    let root = std::env::temp_dir().join("fsys_example_dir_crud");
    let _ = std::fs::remove_dir_all(&root); // ensure clean slate

    // Build a small tree: root/sub/inner/{a.log, b.log, notes.txt}.
    fs.mkdir_all(root.join("sub").join("inner"))?;
    fs.write(root.join("top.log"), b"top")?;
    fs.write(root.join("sub").join("inner").join("a.log"), b"a")?;
    fs.write(root.join("sub").join("inner").join("b.log"), b"b")?;
    fs.write(root.join("sub").join("inner").join("notes.txt"), b"n")?;

    // Flat: only what's directly inside `root`.
    let flat = fs.scan(&root)?;
    println!("scan (non-recursive): {} entries", flat.len());

    // Recursive: every entry under the tree.
    let deep = fs.scan_all(&root)?;
    println!("scan_all (recursive): {} entries", deep.len());

    // Glob find — every .log anywhere under the tree.
    let logs = fs.find(&root, "**/*.log")?;
    println!("find('**/*.log'):     {} matches", logs.len());

    // Recursive count of regular files.
    let count = fs.count_all(&root)?;
    println!("count_all:            {} regular files", count);

    // Cleanup the tree.
    fs.rmdir_all(&root)?;
    Ok(())
}
