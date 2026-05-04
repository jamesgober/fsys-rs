//! Crash-safety integration test for `Method::Sync`.
//!
//! Validates the atomic-replace contract under three kill points:
//! `PreSyscall` (kill before any write), `MidSyscall` (kill while
//! the syscall is in flight — the documented torn-write window),
//! and `PostSyscall` (kill after rename completed).
//!
//! Per D-2 in `.dev/DECISIONS-0.5.0.md`, kill points are
//! deterministic via stdout-line-based synchronisation, not timing-
//! based. The 100× pre-merge stability protocol from D-4 applies to
//! this file.

#[path = "crash_harness/mod.rs"]
mod harness;

use harness::{CrashSpec, KillMode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_crash_sync_{}_{}_{}",
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
        method: fsys::Method::Sync,
        target,
        initial: b"INITIAL_payload_for_atomic_replace_sync_path".to_vec(),
        new: b"NEW_payload_for_atomic_replace_sync_path".to_vec(),
        kill_mode,
    }
}

#[test]
fn crash_sync_pre_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("pre");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::PreSyscall);
    let result = harness::run(spec.clone(), "crash_sync_pre_syscall");
    harness::assert_atomic_replace(&spec, &result);
}

#[test]
fn crash_sync_mid_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("mid");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::MidSyscall { jitter_us: 100 });
    let result = harness::run(spec.clone(), "crash_sync_mid_syscall");
    harness::assert_atomic_replace(&spec, &result);
}

#[test]
fn crash_sync_post_syscall() {
    harness::maybe_run_as_victim_and_exit();
    let path = tmp("post");
    let _g = Cleanup(path.clone());
    let spec = make_spec(path.clone(), KillMode::PostSyscall);
    let result = harness::run(spec.clone(), "crash_sync_post_syscall");
    harness::assert_atomic_replace(&spec, &result);
    // Post-syscall the rename completed before kill — file MUST be
    // the new payload.
    let bytes = result
        .final_state
        .as_ref()
        .expect("post-syscall file must exist");
    assert_eq!(
        bytes, &spec.new,
        "post-syscall kill: file must be the new payload"
    );
}
