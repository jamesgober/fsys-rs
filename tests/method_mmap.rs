//! 0.5.0 integration: `Method::Mmap` end-to-end through the public
//! Handle API, including the documented fallback paths.
//!
//! Validates:
//! - Round-trip write+read with Method::Mmap on a page-or-larger
//!   payload.
//! - Permanent fallback to Method::Sync when the payload is sub-page
//!   (per R-2'' in `.dev/DECISIONS-0.5.0.md`). The Handle's
//!   `active_method()` reflects the fallback after one unsuitable
//!   op.
//! - Permanent fallback when reading a sub-page or zero-byte file.
//! - Atomic-replace contract: file is either entirely-old or
//!   entirely-new at every observable point — never torn.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use fsys::{builder, Method};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_int_mmap_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

struct TmpFile(PathBuf);
impl Drop for TmpFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn page_aligned_payload() -> Vec<u8> {
    let n = fsys::os::info().page_size.max(4096);
    let mut v = vec![0u8; n];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    v
}

#[test]
fn mmap_write_then_read_round_trip() {
    let h = builder().method(Method::Mmap).build().expect("handle");
    let path = tmp("round");
    let _g = TmpFile(path.clone());
    let data = page_aligned_payload();

    h.write(&path, &data).expect("write");
    // Active method should still be Mmap — payload was suitable.
    assert_eq!(h.active_method(), Method::Mmap);

    let actual = h.read(&path).expect("read");
    assert_eq!(actual, data);
    // Read of a page-or-larger file keeps Mmap active.
    assert_eq!(h.active_method(), Method::Mmap);
}

#[test]
fn mmap_write_replaces_existing_atomically() {
    let h = builder().method(Method::Mmap).build().expect("handle");
    let path = tmp("replace");
    let _g = TmpFile(path.clone());

    // Pre-populate with old content.
    std::fs::write(&path, b"old short content").unwrap();

    let new_data = page_aligned_payload();
    h.write(&path, &new_data).expect("write");

    let actual = std::fs::read(&path).expect("read");
    assert_eq!(actual, new_data);
}

#[test]
fn mmap_write_falls_back_to_sync_for_sub_page_payload() {
    let h = builder().method(Method::Mmap).build().expect("handle");
    assert_eq!(h.method(), Method::Mmap);
    assert_eq!(h.active_method(), Method::Mmap);

    let path = tmp("subpage_write");
    let _g = TmpFile(path.clone());
    let small = b"under one page";

    // Fallback fires here — payload is sub-page.
    h.write(&path, small).expect("write");

    // Active method now Sync; subsequent ops use Sync directly.
    assert_eq!(
        h.active_method(),
        Method::Sync,
        "Mmap → Sync fallback must update active_method"
    );

    // The write itself succeeded via the Sync atomic-replace path.
    assert_eq!(std::fs::read(&path).unwrap(), small);

    // A second sub-page write proceeds as Sync directly.
    h.write(&path, b"second small write").expect("second");
    assert_eq!(h.active_method(), Method::Sync);
}

#[test]
fn mmap_write_falls_back_for_zero_length_payload() {
    let h = builder().method(Method::Mmap).build().expect("handle");
    let path = tmp("zerolen");
    let _g = TmpFile(path.clone());

    h.write(&path, &[]).expect("zero-len write");

    assert_eq!(h.active_method(), Method::Sync);
    assert_eq!(std::fs::read(&path).unwrap(), Vec::<u8>::new());
}

#[test]
fn mmap_read_falls_back_for_sub_page_file() {
    let h = builder().method(Method::Mmap).build().expect("handle");
    let path = tmp("subpage_read");
    let _g = TmpFile(path.clone());

    // Write a small file using a fresh non-Mmap handle so the test
    // setup doesn't itself trigger the fallback.
    {
        let setup = builder().method(Method::Sync).build().expect("setup");
        setup
            .write(&path, b"sub-page content")
            .expect("setup write");
    }

    let data = h.read(&path).expect("read");
    assert_eq!(data, b"sub-page content");
    assert_eq!(
        h.active_method(),
        Method::Sync,
        "Mmap → Sync fallback fires on sub-page read"
    );
}

#[test]
fn mmap_read_falls_back_for_zero_byte_file() {
    let h = builder().method(Method::Mmap).build().expect("handle");
    let path = tmp("zerobyte_read");
    let _g = TmpFile(path.clone());
    std::fs::write(&path, b"").unwrap();

    let data = h.read(&path).expect("read");
    assert_eq!(data, Vec::<u8>::new());
    assert_eq!(h.active_method(), Method::Sync);
}

#[test]
fn mmap_active_method_starts_at_mmap_before_first_op() {
    let h = builder().method(Method::Mmap).build().expect("handle");
    // Before any op, active_method is Mmap (resolve passes through
    // for non-Auto requests).
    assert_eq!(h.method(), Method::Mmap);
    assert_eq!(h.active_method(), Method::Mmap);
}

#[test]
fn mmap_atomic_replace_no_torn_writes() {
    // Single-threaded check: between successive page-aligned writes,
    // the file is always either entirely the old payload or entirely
    // the new one. We can't fully prove "no torn writes" without
    // crash injection (that's the H checkpoint's harness), but we
    // can prove the round-trip succeeds and the file is well-formed
    // after each step.
    let h = builder().method(Method::Mmap).build().expect("handle");
    let path = tmp("atomic");
    let _g = TmpFile(path.clone());

    let p1 = page_aligned_payload();
    let mut p2 = p1.clone();
    for b in p2.iter_mut() {
        *b = b.wrapping_add(1);
    }
    let mut p3 = p2.clone();
    for b in p3.iter_mut() {
        *b = b.wrapping_add(1);
    }

    h.write(&path, &p1).expect("w1");
    assert_eq!(h.read(&path).unwrap(), p1);

    h.write(&path, &p2).expect("w2");
    assert_eq!(h.read(&path).unwrap(), p2);

    h.write(&path, &p3).expect("w3");
    assert_eq!(h.read(&path).unwrap(), p3);
}
