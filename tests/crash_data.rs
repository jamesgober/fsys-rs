//! Crash-safety integration test for `Method::Data`.
//!
//! Same shape as `crash_sync.rs`. Method::Data uses `fdatasync` on
//! Linux and falls back to full sync elsewhere; the atomic-replace
//! contract is identical.

#[path = "crash_harness/mod.rs"]
mod harness;

use harness::{CrashSpec, KillMode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_crash_data_{}_{}_{}",
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
    CrashSpec {
        method: fsys::Method::Data,
        target,
        initial: b"INITIAL_payload_data_method_atomic_replace".to_vec(),
        new: b"NEW_payload_data_method_atomic_replace".to_vec(),
        kill_mode,
    }
}

#[test]
fn crash_data_pre_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("pre");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::PreSyscall);
    let result = harness::run(spec.clone(), "crash_data_pre_syscall");
    harness::assert_atomic_replace(&spec, &result);
}

#[test]
fn crash_data_mid_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("mid");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::MidSyscall { jitter_us: 100 });
    let result = harness::run(spec.clone(), "crash_data_mid_syscall");
    harness::assert_atomic_replace(&spec, &result);
}

#[test]
fn crash_data_post_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("post");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::PostSyscall);
    let result = harness::run(spec.clone(), "crash_data_post_syscall");
    harness::assert_atomic_replace(&spec, &result);
    let bytes = result
        .final_state
        .as_ref()
        .expect("post-syscall file must exist");
    assert_eq!(bytes, &spec.new);
}
