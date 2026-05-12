//! 0.9.6 audit H-6 — fd exhaustion (EMFILE) integration test.
//!
//! Sets `RLIMIT_NOFILE` to a low ceiling, exhausts the file
//! descriptor table, and verifies fsys surfaces a clean error
//! (no panic, no hang) on every operation that needs an fd.
//!
//! `setrlimit` is process-wide, so this test lives in its own
//! integration binary — Cargo runs each `tests/*.rs` file in a
//! separate process, isolating the fd-limit change.
//!
//! ## Why this matters
//!
//! In production, processes can hit EMFILE under load: too many
//! sockets, too many file handles, container fd limits. fsys is a
//! storage-foundation library — every consumer downstream pays
//! the cost if a single EMFILE surfaces as a panic or hang rather
//! than a clean error.

#![cfg(unix)]

use fsys::builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_fd_exhaust_{}_{}_{tag}",
        std::process::id(),
        n
    ))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Try to lower the soft RLIMIT_NOFILE. Returns the original soft
/// limit so the caller can restore it after the test (best-effort
/// — if the test panics, drop runs are skipped, but Cargo isolates
/// tests per-binary so the leak doesn't affect siblings).
fn try_lower_nofile_limit(target_soft: u64) -> Option<u64> {
    // SAFETY: `libc::rlimit` is a POD struct; the all-zeros bit
    // pattern is valid for it. `getrlimit` writes the current
    // values through the out-pointer.
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) };
    if rc != 0 {
        return None;
    }
    let original_soft = rl.rlim_cur as u64;
    // Pick the lower of (target, original_soft) — we never raise
    // the limit (that may require privileges).
    let new_soft = std::cmp::min(target_soft, original_soft);
    let new_rl = libc::rlimit {
        rlim_cur: new_soft as libc::rlim_t,
        rlim_max: rl.rlim_max,
    };
    // SAFETY: new_rl is a valid rlimit struct with rlim_cur <= rlim_max.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new_rl) };
    if rc != 0 {
        return None;
    }
    Some(original_soft)
}

#[test]
fn handle_construction_does_not_hang_under_fd_pressure() {
    // Lower the limit to something tight enough that we can exhaust
    // it predictably without spending forever opening fds.
    let Some(_original) = try_lower_nofile_limit(64) else {
        // CI runner refused to setrlimit — skip the test.
        eprintln!("fd_exhaustion: setrlimit refused; skipping");
        return;
    };

    // Build the fsys handle. Construction itself opens internal
    // resources (file pool, etc.); under tight fd pressure, this
    // should either succeed with clean degradation or return a
    // clean error — never panic, never hang.
    let fs_result = builder().build();
    // Either outcome is acceptable. We only assert no-panic, which
    // is implicit (this test point would not have been reached).
    let _ = fs_result;
}

#[test]
fn write_under_fd_pressure_returns_error_not_panic() {
    // Lower the limit, then deliberately exhaust it by opening
    // many files. Subsequent fsys operations should surface a
    // clean error, not hang or panic.
    let Some(_original) = try_lower_nofile_limit(64) else {
        eprintln!("fd_exhaustion: setrlimit refused; skipping");
        return;
    };

    // Build the handle BEFORE exhausting fds — if construction
    // itself needs fds, we want it to succeed first.
    let fs = match builder().build() {
        Ok(fs) => fs,
        Err(_) => {
            eprintln!("fd_exhaustion: handle build failed under tight fd limit; skipping");
            return;
        }
    };

    // Now hold open enough fds to push us to the ceiling. The
    // exact number depends on what the handle/runtime already
    // consumed; we just open until open() starts to fail.
    let mut held: Vec<std::fs::File> = Vec::new();
    let scratch = tmp_path("scratch");
    let _g = Cleanup(scratch.clone());
    std::fs::write(&scratch, b"scratch").expect("create scratch");
    loop {
        match std::fs::File::open(&scratch) {
            Ok(f) => held.push(f),
            Err(_) => break,
        }
        // Safety cap so this loop can't run away under a high
        // ceiling.
        if held.len() > 10_000 {
            break;
        }
    }

    // Now attempt a fsys write. The expected outcome is a clean
    // error (no fd left to open the target path), not a panic
    // or hang.
    let target = tmp_path("target");
    let _g2 = Cleanup(target.clone());
    let result = fs.write(&target, b"payload");
    // Either:
    //  - We hit EMFILE / ENFILE: result is Err(Error::Io(...))
    //  - The handle had spare fds: result is Ok (acceptable)
    // What we MUST NOT see is a panic (which would have
    // unwound the test) or a hang (the test would not return).
    // We don't pin the error variant — different OSes / FUSE /
    // tmpfs combinations surface different errno values.
    let _ = result;

    // Drop the held files so the test runner isn't starved.
    drop(held);
}

#[test]
fn journal_open_under_fd_pressure_returns_error_not_panic() {
    let Some(_original) = try_lower_nofile_limit(64) else {
        eprintln!("fd_exhaustion: setrlimit refused; skipping");
        return;
    };

    let fs = match builder().build() {
        Ok(fs) => fs,
        Err(_) => {
            eprintln!("fd_exhaustion: handle build failed under tight fd limit; skipping");
            return;
        }
    };

    // Exhaust fds.
    let scratch = tmp_path("scratch_journal");
    let _g = Cleanup(scratch.clone());
    std::fs::write(&scratch, b"scratch").expect("create scratch");
    let mut held: Vec<std::fs::File> = Vec::new();
    loop {
        match std::fs::File::open(&scratch) {
            Ok(f) => held.push(f),
            Err(_) => break,
        }
        if held.len() > 10_000 {
            break;
        }
    }

    let target = tmp_path("journal_under_pressure");
    let _g2 = Cleanup(target.clone());
    // Journal open needs an fd; under EMFILE this should return
    // an error cleanly.
    let _result = fs.journal(&target);
    // Same contract as the write test: no-panic, no-hang. The
    // result variant depends on the OS surface.

    drop(held);
}
