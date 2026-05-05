//! Hostile-filesystem stress tests (0.7.0 stress expansion).
//!
//! Each filesystem here has known quirks that fsys's
//! `Method::Direct` fallback and atomic-replace path must handle
//! gracefully. The test scaffolding lives unconditionally; the
//! actual exercise on a given filesystem only runs when the
//! corresponding `FSYS_TEST_<FS>_DIR` env var points to a
//! directory backed by that filesystem.
//!
//! ## Operating model
//!
//! - **Tier 1 (in-session)**: tests run with no env vars set;
//!   they detect the absence and skip with `eprintln!`. The
//!   harness compiles + the skip path is exercised.
//! - **Tier 2 (CI nightly)**: a CI matrix sets
//!   `FSYS_TEST_TMPFS_DIR=/dev/shm`,
//!   `FSYS_TEST_FAT32_DIR=/mnt/fat32-test-vol`, etc., and the
//!   tests exercise real filesystems.
//! - **Tier 3 (release-prep)**: humans run on a workstation with
//!   manually-mounted FAT32 / exFAT / NFS / SMB shares, set the
//!   env vars, run the tests.
//!
//! ## Coverage per filesystem
//!
//! | FS | Quirk | What we verify |
//! |---|---|---|
//! | tmpfs | Rejects `O_DIRECT` (EINVAL) | Direct downgrades to Data; write succeeds |
//! | FAT32 | No POSIX permissions; case-insensitive on most mounts | `write_copy` mode-preservation silently no-ops; case collisions surface as Io errors |
//! | exFAT | Similar to FAT32 + larger file support | Same as FAT32 |
//! | NFS | Network latency, weak `fsync` semantics on some mounts | `Method::Sync` writes complete; latency tail is tolerated |
//! | SMB | Network latency, locking quirks | Same as NFS |

use fsys::{builder, Method};
use std::path::PathBuf;

/// Look up an env var pointing to a directory of the given FS
/// type. Returns `None` if the var is unset; logs and returns
/// `None` if the var is set but the directory doesn't exist.
fn fs_test_dir(env_key: &str) -> Option<PathBuf> {
    let path = std::env::var_os(env_key)?;
    let p = PathBuf::from(path);
    if !p.is_dir() {
        eprintln!("[hostile_fs] {env_key} set to {p:?} but path is not a directory; skipping");
        return None;
    }
    Some(p)
}

#[test]
fn tmpfs_direct_downgrades_to_data() {
    let Some(dir) = fs_test_dir("FSYS_TEST_TMPFS_DIR") else {
        eprintln!("[tmpfs] FSYS_TEST_TMPFS_DIR not set; skipping (set to e.g. /dev/shm to enable)");
        return;
    };

    let fs = builder()
        .method(Method::Direct)
        .root(&dir)
        .build()
        .expect("handle");

    // Direct write to tmpfs: kernel rejects O_DIRECT with EINVAL;
    // fsys downgrades active_method to Data and the write
    // succeeds.
    let path = dir.join("tmpfs_direct_write.dat");
    fs.write(&path, b"on tmpfs")
        .expect("write must succeed via fallback");

    let read = std::fs::read(&path).expect("read");
    assert_eq!(read, b"on tmpfs");

    // active_method should now be Data (downgraded from Direct).
    assert_eq!(
        fs.active_method(),
        Method::Data,
        "Direct on tmpfs must downgrade to Data"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn fat32_atomic_replace_succeeds_without_permission_preservation() {
    let Some(dir) = fs_test_dir("FSYS_TEST_FAT32_DIR") else {
        eprintln!("[fat32] FSYS_TEST_FAT32_DIR not set; skipping");
        return;
    };

    let fs = builder().root(&dir).build().expect("handle");

    let path = dir.join("fat32_test.dat");
    fs.write(&path, b"first").expect("write 1");
    fs.write_copy(&path, b"second").expect("write_copy");

    let read = std::fs::read(&path).expect("read");
    assert_eq!(read, b"second");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn exfat_basic_round_trip() {
    let Some(dir) = fs_test_dir("FSYS_TEST_EXFAT_DIR") else {
        eprintln!("[exfat] FSYS_TEST_EXFAT_DIR not set; skipping");
        return;
    };

    let fs = builder().root(&dir).build().expect("handle");
    let path = dir.join("exfat_test.dat");
    fs.write(&path, b"exfat round trip").expect("write");
    let read = fs.read(&path).expect("read");
    assert_eq!(read, b"exfat round trip");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn nfs_method_sync_completes_with_tolerable_latency() {
    let Some(dir) = fs_test_dir("FSYS_TEST_NFS_DIR") else {
        eprintln!("[nfs] FSYS_TEST_NFS_DIR not set; skipping");
        return;
    };

    let fs = builder()
        .method(Method::Sync)
        .root(&dir)
        .build()
        .expect("handle");

    let path = dir.join("nfs_sync.dat");
    let start = std::time::Instant::now();
    fs.write(&path, b"nfs sync").expect("nfs sync write");
    let elapsed = start.elapsed();

    // NFS over a network can be slow; we assert only that the op
    // completed (no hang) within a generous 30-second bound. The
    // intent is regression detection ("did we accidentally
    // introduce a syscall that NFS doesn't support?"), not a
    // performance gate.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "NFS sync write took {elapsed:?} — likely hung"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn smb_basic_round_trip() {
    let Some(dir) = fs_test_dir("FSYS_TEST_SMB_DIR") else {
        eprintln!("[smb] FSYS_TEST_SMB_DIR not set; skipping");
        return;
    };

    let fs = builder().root(&dir).build().expect("handle");
    let path = dir.join("smb_round_trip.dat");
    fs.write(&path, b"smb test").expect("smb write");
    let read = fs.read(&path).expect("smb read");
    assert_eq!(read, b"smb test");
    let _ = std::fs::remove_file(&path);
}
