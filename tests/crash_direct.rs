//! Crash-safety integration test for `Method::Direct`.
//!
//! Method::Direct uses the existing `O_DIRECT` + `pwrite` +
//! `fdatasync` path on Linux (io_uring deferred to 0.5.x patch
//! per the io_uring blocker in `.dev/DECISIONS-0.5.0.md`),
//! `F_NOCACHE` + `F_FULLFSYNC` on macOS, and
//! `FILE_FLAG_NO_BUFFERING` + `FILE_FLAG_WRITE_THROUGH` on Windows.
//! The atomic-replace contract is identical across all backends.

#[path = "crash_harness/mod.rs"]
mod harness;

use harness::{CrashSpec, KillMode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_crash_direct_{}_{}_{}",
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
    // Direct IO requires sector-aligned (or larger) payloads in
    // some configurations; the platform's atomic-replace path
    // handles alignment internally via `AlignedBuf`. We use a
    // 4 KiB payload which is page-aligned on every supported
    // target.
    let mut initial = vec![0u8; 4096];
    let mut new = vec![0u8; 4096];
    for (i, b) in initial.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    for (i, b) in new.iter_mut().enumerate() {
        *b = ((i + 17) % 251) as u8;
    }
    CrashSpec {
        method: fsys::Method::Direct,
        target,
        initial,
        new,
        kill_mode,
    }
}

#[test]
fn crash_direct_pre_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("pre");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::PreSyscall);
    let result = harness::run(spec.clone(), "crash_direct_pre_syscall");
    harness::assert_atomic_replace(&spec, &result);
}

#[test]
fn crash_direct_mid_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("mid");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::MidSyscall { jitter_us: 200 });
    let result = harness::run(spec.clone(), "crash_direct_mid_syscall");
    harness::assert_atomic_replace(&spec, &result);
}

#[test]
fn crash_direct_post_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("post");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::PostSyscall);
    let result = harness::run(spec.clone(), "crash_direct_post_syscall");
    harness::assert_atomic_replace(&spec, &result);
    let bytes = result
        .final_state
        .as_ref()
        .expect("post-syscall file must exist");
    assert_eq!(bytes, &spec.new);
}
