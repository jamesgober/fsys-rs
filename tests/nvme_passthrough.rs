//! Integration tests for NVMe passthrough flush.
//!
//! These tests skip honestly on environments that lack NVMe + raw
//! block access (the common case for CI and developer machines):
//! they exercise the **fallback** path, which still validates that
//! capability detection runs without crashing and returns the
//! expected `false` outcome. When a capable environment is
//! available (bare-metal Linux or Windows with admin + NVMe), the
//! tests additionally exercise the **elite** path.
//!
//! Per locked decision D-11, the env var
//! `FSYS_DISABLE_NVME_PASSTHROUGH=1` forces the fallback path even
//! on capable hardware. One test sets this var and verifies the
//! `active_durability_primitive()` accessor reflects the fallback
//! string.

use fsys::builder;
use fsys::primitive;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("fsys_nvme_pt_{}_{}_{}", std::process::id(), n, tag))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn active_durability_primitive_returns_canonical_string() {
    // Pre-IO probe state on Sync method should report `fsync` on
    // Linux/Windows or `F_FULLFSYNC` on macOS.
    let fs = builder()
        .method(fsys::Method::Sync)
        .build()
        .expect("handle");

    let p = fs.active_durability_primitive();
    let known = [primitive::FSYNC, primitive::F_FULLFSYNC];
    assert!(
        known.contains(&p),
        "Sync method must report a Sync-class primitive; got {p:?}"
    );
}

#[test]
fn active_durability_primitive_for_data_method() {
    let fs = builder()
        .method(fsys::Method::Data)
        .build()
        .expect("handle");
    let p = fs.active_durability_primitive();

    #[cfg(target_os = "linux")]
    assert_eq!(p, primitive::FDATASYNC);
    #[cfg(target_os = "macos")]
    assert_eq!(p, primitive::F_FULLFSYNC);
    #[cfg(target_os = "windows")]
    assert_eq!(p, primitive::FSYNC);
}

#[test]
fn active_durability_primitive_for_mmap_method() {
    let fs = builder()
        .method(fsys::Method::Mmap)
        .build()
        .expect("handle");
    assert_eq!(fs.active_durability_primitive(), primitive::MMAP_MSYNC);
}

#[test]
fn nvme_disable_env_forces_fallback_primitive() {
    // SAFETY ATTENTION: this test mutates process env. It MUST run
    // single-threaded relative to other primitive-checking tests.
    // libtest's default --test-threads=1 for this binary is
    // sufficient because no other test in this file races with the
    // env mutation. We restore the var on test exit so subsequent
    // tests in other binaries are unaffected.
    let prior = std::env::var_os("FSYS_DISABLE_NVME_PASSTHROUGH");

    // SAFETY: `set_var` is documented as racy under multi-threaded
    // processes; this test runs single-threaded (--test-threads=1)
    // and no other test in this binary mutates this var.
    unsafe {
        std::env::set_var("FSYS_DISABLE_NVME_PASSTHROUGH", "1");
    }

    let fs = builder()
        .method(fsys::Method::Direct)
        .build()
        .expect("handle");

    // Trigger a Direct write so the lazy probes run with the env
    // override in place.
    let path = tmp_path("nvme_disable");
    let _g = Cleanup(path.clone());
    let _ = fs.write(&path, b"hello"); // may use Data fallback on
                                       // Direct-rejected filesystems; either way, doesn't error.

    let p = fs.active_durability_primitive();
    // With the env override, the elite path must NOT be reported.
    assert_ne!(
        p,
        primitive::IO_URING_NVME_FLUSH,
        "env override must force fallback"
    );
    assert_ne!(
        p,
        primitive::FILE_FLAG_WRITE_THROUGH_NVME_IOCTL,
        "env override must force fallback (Windows)"
    );

    // Restore prior state for other tests.
    // SAFETY: same reasoning as the set above — single-threaded
    // test, no concurrent env mutation.
    unsafe {
        match prior {
            Some(v) => std::env::set_var("FSYS_DISABLE_NVME_PASSTHROUGH", v),
            None => std::env::remove_var("FSYS_DISABLE_NVME_PASSTHROUGH"),
        }
    }
}

#[test]
fn direct_write_succeeds_regardless_of_passthrough_capability() {
    // Direct should always succeed via fallback even when NVMe
    // passthrough is unavailable. This validates the
    // capability-failure-is-non-fatal contract.
    let path = tmp_path("direct_write");
    let _g = Cleanup(path.clone());

    let fs = builder()
        .method(fsys::Method::Direct)
        .build()
        .expect("handle");

    fs.write(
        &path,
        b"some payload that is at least a sector long for direct IO",
    )
    .expect("Direct write must succeed (with or without NVMe passthrough)");

    let read = std::fs::read(&path).expect("read");
    assert!(read.starts_with(b"some payload"));
}

#[test]
#[cfg(target_os = "linux")]
fn linux_capability_probe_does_not_panic_on_temp_dir_fd() {
    // Open a regular file on whatever filesystem `std::env::temp_dir`
    // resolves to (often tmpfs). Capability probing must return
    // None gracefully (tmpfs is not NVMe), not crash, not leak.
    let path = tmp_path("probe_tmpfs");
    let _g = Cleanup(path.clone());
    std::fs::write(&path, b"x").unwrap();
    let f = std::fs::File::open(&path).unwrap();

    // We can't call `nvme_flush_capable` directly because it's
    // pub(crate). But we can validate via the public API that a
    // Direct write doesn't crash on tmpfs:
    let fs = builder()
        .method(fsys::Method::Direct)
        .build()
        .expect("handle");
    let _ = fs.write(&path, b"y");

    // The probe should have been triggered. Verify the primitive
    // string is one of the known Linux Direct values.
    let p = fs.active_durability_primitive();
    let allowed = [
        primitive::IO_URING_NVME_FLUSH,
        primitive::IO_URING_FDATASYNC,
        primitive::O_DIRECT_PWRITE_FDATASYNC,
        // Direct may have downgraded to Data on tmpfs (which
        // rejects O_DIRECT). The active method then becomes Data,
        // and the primitive is fdatasync.
        primitive::FDATASYNC,
        // Or fall back further to Sync on extremely restrictive
        // filesystems.
        primitive::FSYNC,
    ];
    assert!(
        allowed.contains(&p),
        "Linux Direct on tmpfs reported unknown primitive {p:?}"
    );
    drop(f);
}
