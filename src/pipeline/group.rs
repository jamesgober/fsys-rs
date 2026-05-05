//! Group-lane dispatcher: bounded MPMC queue + per-handle thread that
//! accumulates [`BatchJob`]s under a hybrid time-or-count window and
//! executes their ops in strict submission order.
//!
//! ## Op-execution model (decision D-4(c))
//!
//! The dispatcher runs without a [`crate::Handle`] reference. Each
//! [`BatchJob`] carries a [`HandleSnapshot`] captured at submit time
//! (method, sector size, use_direct flag). Paths are pre-resolved by
//! the Handle before being placed in [`BatchOp`]s; the dispatcher does
//! not perform path resolution or root-jail enforcement.
//!
//! [`execute_write`] is a leaner extraction of
//! [`crate::crud::file`]'s atomic-replace flow. It mirrors solo-lane
//! semantics — temp file → write → flush → atomic rename → best-effort
//! parent-dir sync — except for the one piece that requires Handle
//! state: when `O_DIRECT` is rejected at open time on a per-op basis,
//! the dispatcher falls back locally for that op but does **not**
//! propagate the fallback back to [`crate::Handle::active_method`].
//! Per-op failure is still observable via [`BatchError::source`]. This
//! is decision D-5 in `.dev/DECISIONS-0.4.0.md`; full cross-lane
//! consistency arrives in `0.5.0`.
//!
//! ## Panic safety
//!
//! Each op execution is wrapped in [`std::panic::catch_unwind`]. A
//! panic inside an op converts to a [`BatchError`] for that op (and
//! subsequent ops in the same job are not attempted, matching the
//! non-panic failure semantics of decision #5). The dispatcher thread
//! itself never unwinds.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{select, Receiver, Sender};

use crate::error::BatchError;
use crate::handle::Handle;
use crate::method::Method;
use crate::platform;
use crate::{Error, Result};

use super::PipelineConfig;

// ─────────────────────────────────────────────────────────────────────────────
// Public(crate) types — exposed to the rest of the crate via the parent
// `pipeline` module so the Handle can build BatchOps and HandleSnapshots.
// ─────────────────────────────────────────────────────────────────────────────

/// One operation queued for the group-lane dispatcher.
///
/// Paths are pre-resolved against the Handle's root before construction.
#[derive(Debug)]
pub(crate) enum BatchOp {
    /// Atomically write `data` to `path`, replacing any existing file.
    Write {
        /// Pre-resolved absolute path.
        path: PathBuf,
        /// Payload bytes. Owned by the dispatcher once submitted.
        data: Vec<u8>,
    },
    /// Idempotent removal of `path` (no error if the file is missing).
    Delete {
        /// Pre-resolved absolute path.
        path: PathBuf,
    },
    /// Copy `src` to `dst`. Both paths are pre-resolved.
    Copy {
        /// Pre-resolved source path.
        src: PathBuf,
        /// Pre-resolved destination path.
        dst: PathBuf,
    },
}

/// Snapshot of the [`Handle`] state needed to execute ops.
///
/// Captured at submit time; travels with the [`BatchJob`] into the
/// dispatcher thread.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HandleSnapshot {
    /// Method that determines flush selection (`Sync` → `fsync`,
    /// `Data` → `fdatasync`, `Direct` → no separate flush on Windows
    /// where `WRITE_THROUGH` already flushed; `fdatasync` /
    /// `F_FULLFSYNC` elsewhere).
    pub method: Method,
    /// Logical sector size for Direct IO alignment.
    pub sector_size: u32,
    /// Whether Direct IO was requested. The dispatcher honours this
    /// per-op; if a particular file's filesystem rejects Direct IO,
    /// the dispatcher falls back locally for that op only.
    pub use_direct: bool,
}

/// Response channel for a batch — sync or async. Locked decision
/// D-5 in `.dev/DECISIONS-0.6.0.md`.
///
/// The dispatcher matches exhaustively on this enum (no catch-all
/// arm). The two variants are routed to the same processing path;
/// the only difference is which channel the result lands on.
pub(crate) enum BatchResponse {
    /// Sync caller — uses [`crossbeam_channel::bounded(1)`].
    Sync(Sender<std::result::Result<(), BatchError>>),
    /// Async caller — uses [`tokio::sync::oneshot`]. Only present
    /// when the `async` Cargo feature is enabled.
    #[cfg(feature = "async")]
    Async(tokio::sync::oneshot::Sender<std::result::Result<(), BatchError>>),
}

impl BatchResponse {
    /// Sends the batch result on the held channel. Discards the
    /// send error (the receiver dropped) — matches the existing
    /// 0.4.0 behaviour where handle-drop-mid-flight is a documented
    /// degenerate case rather than a hard error.
    pub(crate) fn send(self, result: std::result::Result<(), BatchError>) {
        match self {
            BatchResponse::Sync(tx) => {
                let _ = tx.send(result);
            }
            #[cfg(feature = "async")]
            BatchResponse::Async(tx) => {
                let _ = tx.send(result);
            }
        }
    }
}

/// One submitted batch.
///
/// Sent from a producer thread to the dispatcher via
/// [`super::PipelineConfig::batch_queue_max`]-bounded channel.
pub(crate) struct BatchJob {
    /// Ops to execute in submission order.
    pub ops: Vec<BatchOp>,
    /// Snapshot of the Handle's IO config at submit time.
    pub snapshot: HandleSnapshot,
    /// Response channel. Sync or async per [`BatchResponse`].
    pub response: BatchResponse,
}

// ─────────────────────────────────────────────────────────────────────────────
// Dispatcher loop
// ─────────────────────────────────────────────────────────────────────────────

/// Runs the dispatcher until shutdown is signaled or all senders
/// disconnect. Sends a final `()` on `done_tx` when it exits.
///
/// This function is the entry point of the per-handle dispatcher
/// thread (spawned by [`super::spawn_dispatcher`]).
pub(super) fn run_dispatcher(
    config: PipelineConfig,
    job_rx: Receiver<BatchJob>,
    shutdown_rx: Receiver<()>,
    done_tx: Sender<()>,
) {
    'outer: loop {
        // Step 1 — wait for the first job, or shutdown.
        let first = select! {
            recv(job_rx) -> r => match r {
                Ok(job) => job,
                // All senders dropped → exit cleanly.
                Err(_) => break 'outer,
            },
            recv(shutdown_rx) -> _ => {
                drain_remaining(&job_rx);
                break 'outer;
            }
        };

        // Step 2 — accumulate within the time/count window.
        let deadline = Instant::now() + Duration::from_millis(config.batch_window_ms);
        let mut accumulated: Vec<BatchJob> = Vec::with_capacity(8);
        let mut total_ops: usize = first.ops.len();
        accumulated.push(first);

        while total_ops < config.batch_size_max {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            select! {
                recv(job_rx) -> r => match r {
                    Ok(job) => {
                        total_ops += job.ops.len();
                        accumulated.push(job);
                    }
                    // Senders gone — process what we have, then exit at
                    // the top of the outer loop (which will see the
                    // disconnect on the next first-job wait).
                    Err(_) => break,
                },
                recv(shutdown_rx) -> _ => {
                    while let Ok(j) = job_rx.try_recv() {
                        accumulated.push(j);
                    }
                    process_jobs(accumulated);
                    break 'outer;
                },
                default(remaining) => break, // window expired
            }
        }

        // Step 3 — execute.
        process_jobs(accumulated);
    }

    // Final ack to Pipeline::drop. Best-effort; the receiver may have
    // timed out already, in which case the send fails silently.
    let _ = done_tx.send(());
}

/// Drains every job already in the queue and executes them. Used during
/// shutdown so in-flight batches do not get lost.
fn drain_remaining(job_rx: &Receiver<BatchJob>) {
    let mut all = Vec::new();
    while let Ok(job) = job_rx.try_recv() {
        all.push(job);
    }
    if !all.is_empty() {
        process_jobs(all);
    }
}

/// Executes every accumulated [`BatchJob`] in order using
/// [`execute_op`] for each op, sending one per-job result back through
/// each job's response channel.
///
/// Wrapper around [`process_jobs_with`] hard-coded to the production
/// executor. The split exists for testability (decision D-6) — the
/// generic helper lets the panic-safety unit test pass in a panicking
/// closure without contaminating the production dispatch path.
fn process_jobs(jobs: Vec<BatchJob>) {
    process_jobs_with(jobs, execute_op);
}

/// Executes every accumulated [`BatchJob`] in order using `executor`
/// for each op.
///
/// Per decision D-6 in `.dev/DECISIONS-0.4.0.md`, the executor is
/// extracted as a function parameter so the panic-safety unit test
/// can substitute a panicking closure. Production code calls this
/// with [`execute_op`].
///
/// Per-op `catch_unwind` semantics: a panic inside `executor` is
/// caught, the offending op's index becomes `failed_at`, the count of
/// successful ops before it becomes `completed`, and the
/// `BatchError::source` is `Error::Io(std::io::Error::other("batch
/// op panicked"))`. Subsequent ops in the same job are **not**
/// attempted (matches the non-panic failure semantics of decision
/// #5). The dispatcher thread itself never unwinds.
fn process_jobs_with<F>(jobs: Vec<BatchJob>, executor: F)
where
    F: Fn(BatchOp, &HandleSnapshot) -> Result<()>,
{
    for job in jobs {
        let response = job.response;
        let snapshot = job.snapshot;
        let ops = job.ops;
        let mut completed: usize = 0;
        let mut failure: Option<(usize, Error)> = None;

        for (idx, op) in ops.into_iter().enumerate() {
            // Per-op catch_unwind. A panicking op fails its own batch
            // but does NOT take down the dispatcher.
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| executor(op, &snapshot)));
            match res {
                Ok(Ok(())) => completed += 1,
                Ok(Err(e)) => {
                    failure = Some((idx, e));
                    break;
                }
                Err(_panic_payload) => {
                    failure = Some((idx, Error::Io(std::io::Error::other("batch op panicked"))));
                    break;
                }
            }
        }

        let result = match failure {
            None => Ok(()),
            Some((failed_at, e)) => Err(BatchError {
                failed_at,
                completed,
                source: Box::new(e),
            }),
        };
        // BatchResponse handles sync vs. async dispatch internally.
        // Best-effort: receiver may have dropped (handle-drop-mid-
        // flight is a documented degenerate case, not an error).
        response.send(result);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Op execution — leaner extraction of crud::file::write semantics.
// ─────────────────────────────────────────────────────────────────────────────

fn execute_op(op: BatchOp, snapshot: &HandleSnapshot) -> Result<()> {
    match op {
        BatchOp::Write { path, data } => execute_write(&path, &data, snapshot),
        BatchOp::Delete { path } => execute_delete(&path),
        BatchOp::Copy { src, dst } => execute_copy(&src, &dst, snapshot),
    }
}

/// Atomic-replace write. Mirrors [`crate::crud::file`]'s `Handle::write`
/// minus the `update_active_method` callback. See decisions D-1 and
/// D-4(c) in `.dev/DECISIONS-0.4.0.md` for why this duplication exists
/// and where it folds back together in `0.5.0`.
fn execute_write(path: &Path, data: &[u8], snapshot: &HandleSnapshot) -> Result<()> {
    let temp = Handle::gen_temp_path(path);

    // Step 1: open the temp file (Direct IO if requested).
    let (file, direct_ok) = platform::open_write_new(&temp, snapshot.use_direct).map_err(|e| {
        Error::AtomicReplaceFailed {
            step: "open_temp",
            source: as_io_error(e),
        }
    })?;

    // Step 2: write data. Direct IO uses sector-aligned write; buffered
    // path is the fallback.
    let write_result = if direct_ok {
        platform::write_all_direct(&file, data, snapshot.sector_size)
    } else {
        platform::write_all(&file, data)
    };
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::AtomicReplaceFailed {
            step: "write",
            source: as_io_error(e),
        });
    }

    if direct_ok {
        // Step 3 (Direct IO): NO_BUFFERING writes are sector-padded.
        // Drop the NO_BUFFERING handle (WRITE_THROUGH already flushed
        // bytes to disk on Windows; on Linux/macOS the file was just
        // written) and reopen buffered to truncate to the actual data
        // length. Mirrors crud/file.rs:71-84.
        drop(file);
        if let Err(e) = std::fs::OpenOptions::new()
            .write(true)
            .open(&temp)
            .and_then(|f| f.set_len(data.len() as u64))
        {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::AtomicReplaceFailed {
                step: "truncate",
                source: e,
            });
        }
    } else {
        // Step 3 (Buffered path): explicit flush per snapshot.method.
        let flush_result = flush_for_method(&file, snapshot.method);
        if let Err(e) = flush_result {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::AtomicReplaceFailed {
                step: "flush",
                source: as_io_error(e),
            });
        }
        drop(file);
    }

    // Step 5: atomic rename.
    if let Err(e) = platform::atomic_rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::AtomicReplaceFailed {
            step: "rename",
            source: as_io_error(e),
        });
    }

    // Step 6: best-effort parent-dir sync (no-op on Windows).
    let _ = platform::sync_parent_dir(path);

    Ok(())
}

/// Idempotent delete: missing-file is `Ok(())`.
fn execute_delete(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Read-then-write copy. Atomic at the destination via [`execute_write`].
fn execute_copy(src: &Path, dst: &Path, snapshot: &HandleSnapshot) -> Result<()> {
    // Read source via std::fs::read for simplicity. The platform-optimised
    // `copy_file` primitive (copy_file_range / clonefile) is a nice-to-have
    // but does not give atomic-at-destination semantics. The group lane
    // chooses the atomic-replace path for consistency with `Handle::write`.
    let data = std::fs::read(src).map_err(Error::Io)?;
    execute_write(dst, &data, snapshot)
}

/// Selects the flush primitive based on method. Mirrors the
/// `Handle::flush_file` decision tree in `crud/file.rs`.
fn flush_for_method(file: &std::fs::File, method: Method) -> Result<()> {
    match method {
        Method::Direct => {
            // On Windows, FILE_FLAG_WRITE_THROUGH already flushed each
            // write. On Linux/macOS, we still need a fence — fdatasync
            // (Linux) or F_FULLFSYNC (macOS, via sync_data).
            #[cfg(target_os = "windows")]
            {
                Ok(())
            }
            #[cfg(not(target_os = "windows"))]
            {
                platform::sync_data(file)
            }
        }
        Method::Data => platform::sync_data(file),
        // Sync, Auto (resolved), Mmap (reserved), Journal (reserved):
        // full fsync.
        _ => platform::sync_full(file),
    }
}

/// Converts a `crate::Error` into a `std::io::Error` for embedding in
/// `Error::AtomicReplaceFailed { source: std::io::Error }`. Mirrors the
/// helper of the same name in `crud/file.rs`.
fn as_io_error(e: Error) -> std::io::Error {
    match e {
        Error::Io(io_err) => io_err,
        other => std::io::Error::other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_group_test_{}_{}_{}",
            std::process::id(),
            n,
            suffix
        ))
    }

    fn snapshot() -> HandleSnapshot {
        HandleSnapshot {
            method: Method::Sync,
            sector_size: 512,
            use_direct: false,
        }
    }

    struct TmpFile(PathBuf);
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_execute_write_creates_file_with_payload() {
        let path = tmp_path("write_creates");
        let _g = TmpFile(path.clone());
        execute_write(&path, b"payload", &snapshot()).expect("write");
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
    }

    #[test]
    fn test_execute_write_replaces_existing_file() {
        let path = tmp_path("write_replaces");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"old").unwrap();
        execute_write(&path, b"new", &snapshot()).expect("replace");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn test_execute_write_empty_payload_is_valid() {
        let path = tmp_path("empty_write");
        let _g = TmpFile(path.clone());
        execute_write(&path, b"", &snapshot()).expect("empty");
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn test_execute_delete_idempotent_on_missing_file() {
        let path = tmp_path("delete_missing");
        // No file created.
        execute_delete(&path).expect("delete missing should succeed");
    }

    #[test]
    fn test_execute_delete_removes_existing_file() {
        let path = tmp_path("delete_existing");
        std::fs::write(&path, b"x").unwrap();
        execute_delete(&path).expect("delete");
        assert!(!path.exists());
    }

    #[test]
    fn test_execute_copy_duplicates_payload() {
        let src = tmp_path("copy_src");
        let dst = tmp_path("copy_dst");
        let _g1 = TmpFile(src.clone());
        let _g2 = TmpFile(dst.clone());
        std::fs::write(&src, b"copy-payload").unwrap();
        execute_copy(&src, &dst, &snapshot()).expect("copy");
        assert_eq!(std::fs::read(&dst).unwrap(), b"copy-payload");
    }

    #[test]
    fn test_execute_op_dispatches_each_variant() {
        let p1 = tmp_path("op_w");
        let _g1 = TmpFile(p1.clone());
        execute_op(
            BatchOp::Write {
                path: p1.clone(),
                data: b"w".to_vec(),
            },
            &snapshot(),
        )
        .expect("write op");
        assert_eq!(std::fs::read(&p1).unwrap(), b"w");

        execute_op(BatchOp::Delete { path: p1.clone() }, &snapshot()).expect("delete op");
        assert!(!p1.exists());

        let src = tmp_path("op_copy_src");
        let dst = tmp_path("op_copy_dst");
        let _g2 = TmpFile(src.clone());
        let _g3 = TmpFile(dst.clone());
        std::fs::write(&src, b"c").unwrap();
        execute_op(
            BatchOp::Copy {
                src: src.clone(),
                dst: dst.clone(),
            },
            &snapshot(),
        )
        .expect("copy op");
        assert_eq!(std::fs::read(&dst).unwrap(), b"c");
    }

    #[test]
    fn test_execute_write_failure_returns_atomic_replace_error() {
        // Target a directory: open_write_new on a path that *is* a
        // directory must fail.
        let dir = tmp_path("write_to_dir");
        std::fs::create_dir_all(&dir).unwrap();
        let result = execute_write(&dir, b"x", &snapshot());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::AtomicReplaceFailed { step, .. } => {
                assert!(
                    matches!(
                        step,
                        "open_temp" | "write" | "truncate" | "flush" | "rename"
                    ),
                    "unexpected step: {step}"
                );
            }
            other => panic!("expected AtomicReplaceFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_as_io_error_passes_through_io_variant() {
        let inner = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let err = Error::Io(inner);
        let io = as_io_error(err);
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn test_as_io_error_wraps_non_io_variant() {
        let err = Error::HardwareProbeFailed {
            detail: "stub".into(),
        };
        let io = as_io_error(err);
        // The display string of the original error is embedded.
        assert!(io.to_string().contains("FS-00003"));
    }

    #[test]
    fn test_flush_for_method_sync_calls_full_sync() {
        // Smoke test: open a file, flush with Method::Sync.
        let path = tmp_path("flush_sync");
        let _g = TmpFile(path.clone());
        let f = std::fs::File::create(&path).unwrap();
        flush_for_method(&f, Method::Sync).expect("sync flush");
    }

    #[test]
    fn test_flush_for_method_data_calls_data_sync() {
        let path = tmp_path("flush_data");
        let _g = TmpFile(path.clone());
        let f = std::fs::File::create(&path).unwrap();
        flush_for_method(&f, Method::Data).expect("data flush");
    }

    // ── Panic safety (decision D-6) ──────────────────────────────────────
    //
    // Validates the catch_unwind wrapper inside process_jobs_with. We
    // pass a panicking executor and assert: (1) the panic is caught,
    // (2) the response channel sends a BatchError with the right
    // failed_at index, (3) the loop continues to subsequent jobs in the
    // same flush. Per decision D-6, this is the *only* place panic
    // safety is tested at the unit level — the integration file
    // tests/pipeline_panic.rs verifies post-failure recovery via the
    // public API using real (non-panic) errors.

    fn make_job(
        ops: Vec<BatchOp>,
    ) -> (
        BatchJob,
        crossbeam_channel::Receiver<std::result::Result<(), BatchError>>,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let job = BatchJob {
            ops,
            snapshot: snapshot(),
            response: BatchResponse::Sync(tx),
        };
        (job, rx)
    }

    #[test]
    fn test_process_jobs_with_catches_panic_and_reports_batch_error() {
        let (job, rx) = make_job(vec![
            BatchOp::Write {
                path: PathBuf::from("/tmp/ignored-1"),
                data: vec![1],
            },
            BatchOp::Write {
                path: PathBuf::from("/tmp/__panic__"),
                data: vec![2],
            },
            BatchOp::Write {
                path: PathBuf::from("/tmp/never-reached"),
                data: vec![3],
            },
        ]);
        let executor = |op: BatchOp, _snap: &HandleSnapshot| -> Result<()> {
            if let BatchOp::Write { path, .. } = &op {
                if path.to_string_lossy().contains("__panic__") {
                    panic!("test-induced panic for catch_unwind verification");
                }
            }
            Ok(())
        };
        process_jobs_with(vec![job], executor);
        let result = rx.recv().expect("dispatcher must send a response");
        let err = result.expect_err("expected BatchError from panic");
        assert_eq!(err.failed_at, 1, "panic happened at op index 1");
        assert_eq!(err.completed, 1, "op 0 completed before the panic");
        // Source is Error::Io with the panic-marker message.
        match *err.source {
            Error::Io(ref io) => {
                assert!(
                    io.to_string().contains("panicked"),
                    "expected panic marker in error display, got: {io}"
                );
            }
            ref other => panic!("expected Error::Io, got {:?}", other),
        }
    }

    #[test]
    fn test_process_jobs_with_continues_after_panicking_job() {
        // Two jobs in a flush. First job's op panics; second job's ops
        // succeed. The dispatcher must process the second job after
        // catching the first job's panic.
        let (job_panic, rx_panic) = make_job(vec![BatchOp::Write {
            path: PathBuf::from("/tmp/__panic__"),
            data: vec![],
        }]);
        let (job_ok, rx_ok) = make_job(vec![BatchOp::Write {
            path: PathBuf::from("/tmp/ok"),
            data: vec![],
        }]);

        let executor = |op: BatchOp, _snap: &HandleSnapshot| -> Result<()> {
            if let BatchOp::Write { path, .. } = &op {
                if path.to_string_lossy().contains("__panic__") {
                    panic!("test-induced panic");
                }
            }
            Ok(())
        };
        process_jobs_with(vec![job_panic, job_ok], executor);

        let r_panic = rx_panic.recv().expect("first response");
        assert!(r_panic.is_err(), "first job should fail with BatchError");
        let r_ok = rx_ok.recv().expect("second response");
        assert!(
            r_ok.is_ok(),
            "second job should succeed despite first job's panic"
        );
    }

    #[test]
    fn test_process_jobs_with_passes_through_non_panicking_executor() {
        // Sanity: process_jobs_with works as a drop-in for process_jobs
        // when given a non-panicking executor. Confirms the refactor
        // didn't change observable production behaviour.
        let (job, rx) = make_job(vec![
            BatchOp::Write {
                path: PathBuf::from("/tmp/x1"),
                data: vec![],
            },
            BatchOp::Write {
                path: PathBuf::from("/tmp/x2"),
                data: vec![],
            },
        ]);
        let executor = |_op: BatchOp, _snap: &HandleSnapshot| -> Result<()> { Ok(()) };
        process_jobs_with(vec![job], executor);
        let result = rx.recv().expect("response");
        assert!(result.is_ok());
    }
}
