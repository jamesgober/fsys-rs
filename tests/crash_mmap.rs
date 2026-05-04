//! Crash-safety integration test for `Method::Mmap`.
//!
//! Method::Mmap uses the same atomic-replace pattern as
//! [`Method::Sync`] — temp file → `msync` / `FlushViewOfFile` →
//! atomic rename. The only difference from the Sync path is that
//! step 2 (data write) flows through a memory mapping rather than
//! `pwrite(2)`. The atomic-replace contract is identical: file is
//! either entirely-old or entirely-new at every observable point.
//!
//! Payloads here are page-aligned (>= page size) to avoid the mmap
//! path falling back to Sync per R-2'' in
//! `.dev/DECISIONS-0.5.0.md`.

#[path = "crash_harness/mod.rs"]
mod harness;

use harness::{CrashSpec, KillMode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_crash_mmap_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn make_spec(target: PathBuf, kill_mode: KillMode) -> CrashSpec {
    // Page-aligned payloads ensure the mmap path is exercised
    // (otherwise Method::Mmap would fall back to Sync per R-2'').
    let page = fsys::os::info().page_size.max(4096);
    let mut initial = vec![0u8; page];
    let mut new = vec![0u8; page];
    for (i, b) in initial.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    for (i, b) in new.iter_mut().enumerate() {
        *b = ((i + 31) % 251) as u8;
    }
    CrashSpec {
        method: fsys::Method::Mmap,
        target,
        initial,
        new,
        kill_mode,
    }
}

#[test]
fn crash_mmap_pre_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("pre");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::PreSyscall);
    let result = harness::run(spec.clone(), "crash_mmap_pre_syscall");
    harness::assert_atomic_replace(&spec, &result);
}

#[test]
fn crash_mmap_mid_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("mid");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::MidSyscall { jitter_us: 200 });
    let result = harness::run(spec.clone(), "crash_mmap_mid_syscall");
    harness::assert_atomic_replace(&spec, &result);
}

#[test]
fn crash_mmap_post_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("post");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::PostSyscall);
    let result = harness::run(spec.clone(), "crash_mmap_post_syscall");
    harness::assert_atomic_replace(&spec, &result);
    let bytes = result
        .final_state
        .as_ref()
        .expect("post-syscall file must exist");
    assert_eq!(bytes, &spec.new);
}
