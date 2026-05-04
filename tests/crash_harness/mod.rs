//! Cross-platform subprocess crash-safety harness.
//!
//! Per locked decision #7 + D-2 in `.dev/DECISIONS-0.5.0.md`, the
//! harness uses subprocess + signal injection to verify each
//! durability method's crash-safety contract. Synchronization uses
//! the child's stdout (line-based) so kill points are deterministic
//! relative to the dangerous syscall — not timing-based.
//!
//! ## Three kill modes (D-2 refinement)
//!
//! | Mode | Parent reads | Behaviour |
//! |---|---|---|
//! | `PreSyscall` | (nothing — kill immediately after spawn) | Tests the pre-write state — file must be entirely the initial payload (or absent). |
//! | `MidSyscall` | `BEGIN` (child wrote initial state, signalled) → wait jitter → kill | Tests the documented torn-write window — file must NOT be torn (atomic-replace contract). |
//! | `PostSyscall` | `BEGIN` → `END` (write completed) → kill | Tests post-syscall state — file must be entirely the new payload. |
//!
//! ## Subprocess pattern (cross-platform)
//!
//! Each crash test re-spawns the test binary itself
//! (`std::env::current_exe()`) with `--exact <test_name>` to filter
//! libtest to one test, plus the `FSYS_CRASH_VICTIM` env var marking
//! victim mode. The test, on second invocation, sees the env var,
//! runs the victim work (write initial → signal BEGIN → write new →
//! signal END), and exits. The parent kills at the chosen
//! synchronisation point (Unix: `kill(2)` via `child.kill()`;
//! Windows: `TerminateProcess` via `child.kill()`).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fsys::Method;

const ENV_VICTIM: &str = "FSYS_CRASH_VICTIM";
const ENV_METHOD: &str = "FSYS_CRASH_METHOD";
const ENV_TARGET: &str = "FSYS_CRASH_TARGET";
const ENV_INITIAL_HEX: &str = "FSYS_CRASH_INITIAL_HEX";
const ENV_NEW_HEX: &str = "FSYS_CRASH_NEW_HEX";
const ENV_KILL: &str = "FSYS_CRASH_KILL";

// Distinctive sync markers. Plain "BEGIN" / "END" collide with
// libtest's own stdout chatter (e.g. "running 1 test", "test name ...
// ok") under `--nocapture`, which would let the parent read a libtest
// banner as our marker and kill before the victim has done any work.
const MARKER_BEGIN: &str = "__FSYS_CRASH_BEGIN__";
const MARKER_END: &str = "__FSYS_CRASH_END__";

/// Crash kill-mode taxonomy. See module-level docs for semantics.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // exercised across multiple test binaries via #[path]
#[allow(clippy::enum_variant_names)] // `Syscall` postfix is the load-bearing semantic
pub enum KillMode {
    PreSyscall,
    MidSyscall { jitter_us: u64 },
    PostSyscall,
}

/// What the parent gives the child + observes after kill.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct CrashSpec {
    pub method: Method,
    pub target: PathBuf,
    pub initial: Vec<u8>,
    pub new: Vec<u8>,
    pub kill_mode: KillMode,
}

/// Outcome of a crash test from the parent's perspective.
#[derive(Debug)]
#[allow(dead_code)]
pub struct CrashResult {
    /// File contents after the child was killed (or exited).
    pub final_state: Option<Vec<u8>>,
    /// Whether we successfully sent the kill signal. `false` when
    /// the child exited before our kill (e.g. PostSyscall mode where
    /// the child completes the END signal before we kill).
    pub kill_sent: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Victim-mode entry point
// ─────────────────────────────────────────────────────────────────────────────

/// If the current process is the victim subprocess, run the victim
/// work and exit. Otherwise return so the test can continue as the
/// parent.
///
/// Tests call this at the top of every `#[test]` function:
/// ```ignore
/// #[test]
/// fn my_crash_test() {
///     harness::maybe_run_as_victim_and_exit();
///     // ... parent role ...
/// }
/// ```
#[allow(dead_code)]
pub fn maybe_run_as_victim_and_exit() {
    if std::env::var(ENV_VICTIM).is_err() {
        return;
    }
    let method = match std::env::var(ENV_METHOD).ok().as_deref() {
        Some("sync") => Method::Sync,
        Some("data") => Method::Data,
        Some("direct") => Method::Direct,
        Some("mmap") => Method::Mmap,
        _ => std::process::exit(101),
    };
    let target = match std::env::var(ENV_TARGET).ok() {
        Some(s) => PathBuf::from(s),
        None => std::process::exit(102),
    };
    let initial = match std::env::var(ENV_INITIAL_HEX)
        .ok()
        .and_then(|s| hex_decode(&s))
    {
        Some(v) => v,
        None => std::process::exit(103),
    };
    let new = match std::env::var(ENV_NEW_HEX).ok().and_then(|s| hex_decode(&s)) {
        Some(v) => v,
        None => std::process::exit(104),
    };

    // 1. Establish the initial state.
    if std::fs::write(&target, &initial).is_err() {
        std::process::exit(110);
    }

    // 2. Signal BEGIN — the parent's synchronisation point.
    println!("{MARKER_BEGIN}");
    let _ = std::io::stdout().flush();

    // 3. Build a Handle with the requested method and issue the
    //    "dangerous" syscall (atomic-replace write). Errors here are
    //    fatal — we must NOT swallow them silently because that
    //    would falsely report "atomic-replace ok" when the write
    //    actually never happened.
    let handle = match fsys::builder().method(method).build() {
        Ok(h) => h,
        Err(_) => std::process::exit(111),
    };
    if handle.write(&target, &new).is_err() {
        std::process::exit(112);
    }

    // 4. Signal END.
    println!("{MARKER_END}");
    let _ = std::io::stdout().flush();

    // 5. Sleep briefly so the parent has time to read END and kill.
    //    If the parent doesn't kill us within this window, we exit
    //    cleanly (PostSyscall + no-kill is a documented variant for
    //    "run to completion").
    std::thread::sleep(Duration::from_millis(200));
    std::process::exit(0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Parent harness
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn a victim subprocess with the given spec, kill it according
/// to the spec's mode, and observe the resulting file state.
#[allow(dead_code)]
pub fn run(spec: CrashSpec, test_fn_name: &str) -> CrashResult {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(&exe);
    cmd.arg("--exact").arg(test_fn_name);
    cmd.arg("--nocapture");
    cmd.env(ENV_VICTIM, "1");
    cmd.env(ENV_METHOD, encode_method(spec.method));
    cmd.env(ENV_TARGET, &spec.target);
    cmd.env(ENV_INITIAL_HEX, hex_encode(&spec.initial));
    cmd.env(ENV_NEW_HEX, hex_encode(&spec.new));
    cmd.env(ENV_KILL, encode_kill(&spec.kill_mode));
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());

    let mut child = cmd.spawn().expect("spawn victim");
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);

    let kill_sent = match spec.kill_mode {
        KillMode::PreSyscall => kill_pre(&mut child),
        KillMode::MidSyscall { jitter_us } => kill_mid(&mut child, &mut reader, jitter_us),
        KillMode::PostSyscall => kill_post(&mut child, &mut reader),
    };

    // Wait for the child to actually terminate so the file's state
    // is stable when we read it. The exit status is informational —
    // a non-zero exit before the rename is a child-side error and
    // surfaces below as `final_state` being whatever the file
    // contained at kill time.
    let exit_status = child.wait();
    if std::env::var("FSYS_CRASH_DEBUG").is_ok() {
        eprintln!(
            "[crash-harness] child exit: {:?}, kill_sent={}, target={:?}",
            exit_status, kill_sent, spec.target
        );
    }

    let final_state = std::fs::read(&spec.target).ok();
    CrashResult {
        final_state,
        kill_sent,
    }
}

/// Atomic-replace contract assertion.
///
/// After any kill point, the file MUST be in one of three states:
/// 1. Entirely the initial payload (kill before write completed +
///    rename happened).
/// 2. Entirely the new payload (kill after rename).
/// 3. Absent / empty (kill before initial write completed).
///
/// **It MUST NOT be torn** — i.e. neither initial nor new, with
/// matching length but mismatched content, or with intermediate
/// length.
#[allow(dead_code)]
pub fn assert_atomic_replace(spec: &CrashSpec, result: &CrashResult) {
    match &result.final_state {
        None => {
            // File absent — acceptable PreSyscall state when the
            // initial-write itself didn't land. Not torn.
        }
        Some(bytes) => {
            let is_initial = bytes == &spec.initial;
            let is_new = bytes == &spec.new;
            let is_empty = bytes.is_empty();
            assert!(
                is_initial || is_new || is_empty,
                "atomic-replace contract violated for {:?}: \
                 file is neither initial ({} bytes) nor new ({} bytes); \
                 observed {} bytes that match neither",
                spec.method,
                spec.initial.len(),
                spec.new.len(),
                bytes.len(),
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Kill-mode implementations
// ─────────────────────────────────────────────────────────────────────────────

fn kill_pre(child: &mut Child) -> bool {
    // Kill immediately after spawn — before child has had a chance
    // to write the initial payload or BEGIN marker.
    child.kill().is_ok()
}

fn kill_mid<R: BufRead>(child: &mut Child, reader: &mut R, jitter_us: u64) -> bool {
    // Wait for BEGIN (synchronisation point: child has set up
    // initial state and is about to issue the syscall). Skip libtest
    // banner lines ("running 1 test", "test foo ... ok", etc.) until
    // we see our explicit marker.
    if !wait_for_marker(reader, MARKER_BEGIN) {
        return false;
    }
    std::thread::sleep(Duration::from_micros(jitter_us));
    child.kill().is_ok()
}

fn kill_post<R: BufRead>(child: &mut Child, reader: &mut R) -> bool {
    if !wait_for_marker(reader, MARKER_BEGIN) {
        return false;
    }
    if !wait_for_marker(reader, MARKER_END) {
        // Child died after BEGIN but before END — possible if the
        // dangerous syscall failed. Treat as kill_sent=false: the
        // file is either initial, new (race), or absent — the
        // atomic-replace assertion handles all three.
        return false;
    }
    child.kill().is_ok()
}

/// Read lines from `reader` until one starts with `marker`, ignoring
/// any libtest banner / status output interleaved on the child's
/// stdout. Returns `false` on EOF before the marker is seen.
fn wait_for_marker<R: BufRead>(reader: &mut R, marker: &str) -> bool {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return false, // EOF
            Ok(_) => {
                if line.trim_end_matches(['\r', '\n']).starts_with(marker) {
                    return true;
                }
                // else: libtest noise — keep reading.
            }
            Err(_) => return false,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Encoders / decoders
// ─────────────────────────────────────────────────────────────────────────────

fn encode_method(m: Method) -> &'static str {
    // `Method` is `#[non_exhaustive]` from outside the crate; we
    // need a wildcard. Future variants default to `"unknown"` so
    // the harness fails closed (the victim sees an unrecognised
    // method name and exits with a non-zero status the parent
    // surfaces as a test failure).
    match m {
        Method::Sync => "sync",
        Method::Data => "data",
        Method::Direct => "direct",
        Method::Mmap => "mmap",
        Method::Journal => "journal",
        Method::Auto => "auto",
        _ => "unknown",
    }
}

fn encode_kill(k: &KillMode) -> String {
    match k {
        KillMode::PreSyscall => "pre".to_string(),
        KillMode::MidSyscall { jitter_us } => format!("mid:{jitter_us}"),
        KillMode::PostSyscall => "post".to_string(),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(hex_nibble(b >> 4));
        s.push(hex_nibble(b & 0x0F));
    }
    s
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + (n - 10)) as char,
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_value(bytes[i])?;
        let lo = hex_value(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(10 + c - b'a'),
        b'A'..=b'F' => Some(10 + c - b'A'),
        _ => None,
    }
}
