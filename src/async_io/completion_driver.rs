//! Native io_uring async substrate: owner task that drives both
//! submission and completion for a per-handle (or per-journal)
//! async ring.
//!
//! ## Architecture (one fused task, not separate submitter/driver)
//!
//! Per `.dev/DECISIONS-0.7.0.md` §`Native io_uring async substrate`:
//! a single tokio task owns the `io_uring::IoUring` value on its
//! stack frame and fuses submission and completion into one
//! `tokio::select!` loop:
//!
//! 1. Pull ops from the submission `mpsc` channel (callers submit
//!    via [`AsyncIoUring::submit`]), push their SQEs and submit them
//!    to the kernel in one batch.
//! 2. `.await` on `AsyncFd<EventFd>`, which becomes readable when
//!    the kernel posts CQEs.
//! 3. Drain the CQ and route each result to its op's `oneshot`.
//!
//! ## Buffer and fd ownership
//!
//! A future can be dropped at any `.await`. If an SQE only borrowed
//! the caller's buffer, a cancelled `write_async` would free memory
//! the kernel is still reading, and a cancelled caller could close
//! the fd before the SQE was consumed, letting an unrelated `open`
//! reuse the number. So every [`Op`] **moves** its buffer
//! ([`IoBuf`]) and a keep-alive for its open file ([`FileRef`]) into
//! the driver. The driver holds both until the op's final CQE and
//! drops them before replying. Dropping the caller's future
//! therefore never frees in-flight memory and never closes an
//! in-flight fd; the op simply finishes without anyone waiting.
//!
//! If the owner task itself stops with ops still in flight (runtime
//! shutdown, panic, or an abort), the remaining buffers and file
//! keep-alives are leaked instead of freed, because the kernel may
//! still touch them.
//!
//! ## Short writes and flow control
//!
//! A write that completes short is resubmitted for the remainder by
//! the driver, so once queued it finishes (or fails) as a unit
//! whether or not the caller still waits. The driver keeps at most
//! one CQ ring's worth of ops in flight and stops taking new ops
//! while full, so the completion queue cannot overflow.
//!
//! ## Panic resilience
//!
//! If the owner task panics, unwinding drops every pending
//! `oneshot::Sender` (awaiting callers see `RecvError`, which
//! [`AsyncIoUring::submit`] turns into [`Error::HandlePoisoned`]) and
//! the `mpsc::Receiver` (later sends fail and surface as
//! [`Error::CompletionDriverDead`]). `submit` records either case in
//! the `poisoned` flag so subsequent submits short-circuit.
//!
//! ## Lifecycle
//!
//! - Constructed lazily on the first native-substrate op. The
//!   constructor synchronously probes `io_uring_setup(2)` and
//!   `eventfd(2)` so that capability failure surfaces as
//!   `Error::IoUringSetupFailed` rather than from a dangling task.
//! - Dropping [`AsyncIoUring`] closes the submission channel. The
//!   owner task finishes the ops already in flight and then exits;
//!   it is not aborted, since aborting would strand in-flight
//!   buffers.

#![cfg(all(target_os = "linux", feature = "async"))]
#![allow(dead_code)] // Same ICE-class workaround as `linux_iouring.rs` —
                     // any item referencing `io_uring::IoUring` plus a
                     // dead-code lint pass triggers rustc 1.95's
                     // `check_mod_deathness` panic; module-level allow
                     // sidesteps the buggy lint without affecting
                     // correctness (everything here is reachable from
                     // `Handle::async_io_uring`).

use crate::platform::linux_iouring::MAX_SQE_LEN;
use crate::platform::AlignedBuf;
use crate::{Error, Result};
use std::any::Any;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};

/// Delay before the owner retries a submission the kernel refused
/// with a transient error (`EAGAIN` / `EBUSY`). The SQEs stay queued
/// in the meantime.
const SUBMIT_RETRY: Duration = Duration::from_millis(1);

/// Reply channel for one op: bytes written (0 for fsync) or the
/// kernel's error.
pub(crate) type Reply = oneshot::Sender<std::io::Result<usize>>;

/// A buffer the driver owns while the kernel may access it.
///
/// Both variants keep their bytes in a separate heap allocation, so
/// moving an `IoBuf` (into the channel, into the pending map) never
/// moves the memory an SQE points at.
pub(crate) enum IoBuf {
    /// Plain heap buffer (journal frames, buffered-mode writes).
    Vec(Vec<u8>),
    /// Sector-aligned buffer for `O_DIRECT` writes.
    Aligned(AlignedBuf),
}

impl IoBuf {
    fn as_slice(&self) -> &[u8] {
        match self {
            IoBuf::Vec(v) => v,
            IoBuf::Aligned(b) => b.as_slice(),
        }
    }
}

/// Keeps an fd's open file alive while an op that names it is in
/// flight.
///
/// The descriptor stays valid for as long as this value (and so the
/// `owner` it holds) lives. The driver drops it only after the op's
/// final CQE, so a caller that gives up early cannot close the fd
/// under the kernel.
pub(crate) struct FileRef {
    fd: RawFd,
    _owner: Arc<dyn Any + Send + Sync>,
}

impl FileRef {
    /// Wraps `owner`, reading the descriptor to use with `fd_of`.
    /// `fd_of` must return a descriptor that `owner` keeps open.
    pub(crate) fn new<T: Any + Send + Sync>(
        owner: Arc<T>,
        fd_of: impl FnOnce(&T) -> RawFd,
    ) -> Self {
        let fd = fd_of(&owner);
        Self { fd, _owner: owner }
    }
}

/// Op submitted to the owner task.
pub(crate) enum Op {
    /// Write all of `buf` at `offset`. The reply carries the bytes
    /// written, which is below `buf`'s length only when the kernel
    /// reported zero progress.
    Write {
        file: FileRef,
        buf: IoBuf,
        offset: u64,
        reply: Reply,
    },
    /// `fsync` with `IORING_FSYNC_DATASYNC` (same durability as
    /// `fdatasync(2)`).
    Fdatasync { file: FileRef, reply: Reply },
}

/// Handle to the native async substrate's owner task.
///
/// Owns the submission `mpsc::Sender` and the `poisoned` flag. The
/// owner task is detached: it exits on its own once this value is
/// dropped and every in-flight op has completed.
pub(crate) struct AsyncIoUring {
    /// Submission channel. `mpsc::UnboundedSender` is `Send + Sync`
    /// and supports concurrent `send` from many callers without a
    /// lock. Unbounded is safe here because the owner stops pulling
    /// ops while the ring is full; queued ops wait in the channel.
    submit_tx: mpsc::UnboundedSender<Op>,
    /// Set by [`AsyncIoUring::submit`] when it observes that the
    /// owner task is gone (channel closed or a reply sender dropped
    /// without an answer). Later submits short-circuit on it.
    poisoned: AtomicBool,
    /// Owner task handle, kept only so tests can abort the owner to
    /// simulate a panic or wait for it to exit.
    #[cfg(test)]
    join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AsyncIoUring {
    /// Constructs a new async ring + driver. Synchronously probes
    /// `io_uring_setup(2)` and `eventfd(2)` so capability failure
    /// surfaces here rather than from a dangling task.
    ///
    /// Spawns the owner task on the current tokio runtime; must be
    /// called from inside a runtime context.
    pub(crate) fn new(queue_depth: u32) -> Result<Self> {
        // 0.9.4: probe with the setup flags the host kernel supports.
        // 0.9.6: RingMode::Async excludes DEFER_TASKRUN (the kernel
        // would not post CQEs without an explicit GETEVENTS enter)
        // and SINGLE_ISSUER (tokio may move this task between
        // threads). See `RingMode` in iouring_features.rs.
        let mut probe_builder = io_uring::IoUring::builder();
        crate::platform::iouring_features::apply(
            &mut probe_builder,
            crate::platform::iouring_features::RingMode::Async,
        );
        match probe_builder.build(queue_depth) {
            Ok(_probe) => {}
            Err(source) => return Err(Error::IoUringSetupFailed { source }),
        }

        // Probe eventfd construction synchronously too. The task
        // takes ownership of the raw fd and wraps it again so the
        // eventfd is closed exactly once when the task exits.
        let eventfd_raw = create_eventfd()?.into_raw_fd();

        let (tx, rx) = mpsc::unbounded_channel::<Op>();
        let join = tokio::task::spawn(owner_loop(queue_depth, eventfd_raw, rx));
        // Detach outside tests: the task ends by itself once the
        // channel closes and its in-flight ops are done.
        #[cfg(not(test))]
        drop(join);

        Ok(Self {
            submit_tx: tx,
            poisoned: AtomicBool::new(false),
            #[cfg(test)]
            join: std::sync::Mutex::new(Some(join)),
        })
    }

    /// Returns `true` once a submit has observed that the owner task
    /// is gone.
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Builds an op around a fresh reply channel, hands it to the
    /// owner task and `.await`s the result.
    ///
    /// The op (with its buffer and file keep-alive) is queued
    /// synchronously on the first poll. Dropping the returned future
    /// after that does not cancel the op; it completes in the
    /// background and its result is discarded.
    pub(crate) async fn submit(&self, make_op: impl FnOnce(Reply) -> Op) -> Result<usize> {
        if self.is_poisoned() {
            return Err(Error::HandlePoisoned {
                reason: "io_uring completion driver panicked".to_string(),
            });
        }
        let (tx, rx) = oneshot::channel();
        // Channel closed: the owner task is gone (typically a panic).
        // Mark poisoned so future submits short-circuit.
        if self.submit_tx.send(make_op(tx)).is_err() {
            self.poisoned.store(true, Ordering::Release);
            return Err(Error::CompletionDriverDead);
        }
        match rx.await {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => Err(Error::Io(e)),
            Err(_recv_err) => {
                // The owner dropped this op's sender without
                // answering, which only happens when the task
                // unwinds or is torn down mid-op.
                self.poisoned.store(true, Ordering::Release);
                Err(Error::HandlePoisoned {
                    reason: "io_uring completion driver dropped sender".to_string(),
                })
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Owner task
// ─────────────────────────────────────────────────────────────────────────────

/// An op the kernel may still be working on.
struct InFlight {
    reply: Reply,
    file: FileRef,
    /// `None` for fsync.
    buf: Option<IoBuf>,
    /// Bytes already written (writes only).
    done: usize,
    /// File offset of `buf[0]`.
    offset: u64,
}

/// In-flight ops keyed by SQE `user_data`.
#[derive(Default)]
struct PendingOps(HashMap<u64, InFlight>);

impl PendingOps {
    fn track(&mut self, id: u64, op: InFlight) {
        // Ids come from a 64-bit counter, so a live id is never
        // reissued; the previous value is always `None`.
        let _previous = self.0.insert(id, op);
    }
}

impl Drop for PendingOps {
    fn drop(&mut self) {
        // Entries are left only when the owner stops abnormally
        // (runtime shutdown, panic, abort). The kernel may still read
        // these buffers or use these fds, so leak them rather than
        // free them. Dropping each `reply` wakes its caller with an
        // error.
        for (_, op) in self.0.drain() {
            std::mem::forget(op.buf);
            std::mem::forget(op.file);
            drop(op.reply);
        }
    }
}

/// Owner task body. Owns the ring, the eventfd (via `AsyncFd`) and
/// the pending-op table; see the module docs for the loop shape.
async fn owner_loop(queue_depth: u32, eventfd_raw: RawFd, mut rx: mpsc::UnboundedReceiver<Op>) {
    // Wrap the eventfd in `OwnedFd` before anything fallible so every
    // exit path closes it exactly once.
    //
    // SAFETY: `eventfd_raw` is a valid eventfd produced by
    // `create_eventfd` (which used `OwnedFd::into_raw_fd` to release
    // ownership) and not duplicated anywhere else. We are the sole
    // owner from this point onward.
    let owned_fd = unsafe { OwnedFd::from_raw_fd(eventfd_raw) };

    // Rebuild the ring with the flags the probe in
    // `AsyncIoUring::new` accepted.
    let mut builder = io_uring::IoUring::builder();
    crate::platform::iouring_features::apply(
        &mut builder,
        crate::platform::iouring_features::RingMode::Async,
    );
    let Ok(mut ring) = builder.build(queue_depth) else {
        return; // owned_fd drops, eventfd closes once
    };
    // Every SQE carries the caller's raw fd (`types::Fd`), kept open
    // by the op's `FileRef`. 1.1.1 removed the `IORING_REGISTER_FILES`
    // slot cache: it was keyed by fd number and never invalidated, so
    // a closed-then-reused fd number kept resolving to the old file.
    if ring
        .submitter()
        .register_eventfd(owned_fd.as_raw_fd())
        .is_err()
    {
        return;
    }
    let Ok(async_fd) = AsyncFd::with_interest(owned_fd, tokio::io::Interest::READABLE) else {
        return;
    };

    let max_in_flight = ring.completion().capacity();
    let mut pending = PendingOps::default();
    let mut next_id: u64 = 0;
    let mut closing = false;

    loop {
        if closing && pending.0.is_empty() {
            return;
        }
        let backlog = !ring.submission().is_empty();
        tokio::select! {
            biased;

            maybe_op = rx.recv(), if !closing && pending.0.len() < max_in_flight => {
                match maybe_op {
                    Some(op) => {
                        next_id = next_id.wrapping_add(1);
                        start_op(&mut ring, &mut pending, next_id, op);
                        // Batch whatever else is already queued into
                        // the same `io_uring_enter`.
                        while pending.0.len() < max_in_flight {
                            let Ok(op) = rx.try_recv() else { break };
                            next_id = next_id.wrapping_add(1);
                            start_op(&mut ring, &mut pending, next_id, op);
                        }
                    }
                    // Every `AsyncIoUring` handle is gone. Finish the
                    // ops in flight, then exit.
                    None => closing = true,
                }
            }

            ready = async_fd.readable() => {
                // An error here means the runtime's IO driver is
                // shutting down; nothing more can be awaited.
                let Ok(mut guard) = ready else { return };
                clear_eventfd(guard.get_inner().as_raw_fd());
                drain_completions(&mut ring, &mut pending);
                guard.clear_ready();
            }

            () = tokio::time::sleep(SUBMIT_RETRY), if backlog => {}
        }
        if !ring.submission().is_empty() {
            // A refusal (`EAGAIN` / `EBUSY`) leaves the SQEs queued;
            // the `backlog` branch above retries after
            // `SUBMIT_RETRY`. The ops' buffers stay owned by
            // `pending` either way.
            let _ = ring.submit();
        }
    }
}

/// Builds the SQE for the next chunk of `op` (the whole op for
/// fsync). Returns `None` only when the file offset would overflow.
fn sqe_for(id: u64, op: &InFlight) -> Option<io_uring::squeue::Entry> {
    use io_uring::{opcode, types};
    let fd = types::Fd(op.file.fd);
    let Some(buf) = op.buf.as_ref() else {
        return Some(
            opcode::Fsync::new(fd)
                .flags(types::FsyncFlags::DATASYNC)
                .build()
                .user_data(id),
        );
    };
    let rest = buf.as_slice().get(op.done..)?;
    let offset = op.offset.checked_add(u64::try_from(op.done).ok()?)?;
    // `min` bounds the chunk by MAX_SQE_LEN, which fits in a u32.
    let len = u32::try_from(rest.len().min(MAX_SQE_LEN)).ok()?;
    Some(
        opcode::Write::new(fd, rest.as_ptr(), len)
            .offset(offset)
            .build()
            .user_data(id),
    )
}

/// Pushes the next SQE for `op` and records it as pending, or
/// replies with an error if it cannot be queued.
fn submit_or_fail(ring: &mut io_uring::IoUring, pending: &mut PendingOps, id: u64, op: InFlight) {
    let Some(entry) = sqe_for(id, &op) else {
        finish(op, Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
        return;
    };
    // SAFETY: `entry` points into `op.buf`'s heap allocation (stable
    // across moves of the `IoBuf`) and names `op.file`'s fd, which
    // `op.file` keeps open. `op` goes into `pending` right after a
    // successful push and is released only after its CQE has been
    // reaped (or leaked by `PendingOps::drop`), so the memory and the
    // fd outlive the kernel's use of them.
    let pushed = unsafe { ring.submission().push(&entry) }.is_ok() || {
        // SQ full: hand what is queued to the kernel, then retry
        // once. A refused submit keeps the SQ full and we fail the
        // op below without the kernel ever seeing it.
        let _ = ring.submit();
        // SAFETY: same as the first push.
        unsafe { ring.submission().push(&entry) }.is_ok()
    };
    if pushed {
        pending.track(id, op);
    } else {
        finish(op, Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    }
}

/// Accepts a new op from the channel.
fn start_op(ring: &mut io_uring::IoUring, pending: &mut PendingOps, id: u64, op: Op) {
    let in_flight = match op {
        Op::Write {
            file,
            buf,
            offset,
            reply,
        } => InFlight {
            reply,
            file,
            buf: Some(buf),
            done: 0,
            offset,
        },
        Op::Fdatasync { file, reply } => InFlight {
            reply,
            file,
            buf: None,
            done: 0,
            offset: 0,
        },
    };
    if in_flight
        .buf
        .as_ref()
        .is_some_and(|b| b.as_slice().is_empty())
    {
        finish(in_flight, Ok(0));
        return;
    }
    submit_or_fail(ring, pending, id, in_flight);
}

/// Releases `op`'s buffer and file keep-alive, then delivers
/// `result`. The kernel is done with both by the time this runs.
fn finish(op: InFlight, result: std::io::Result<usize>) {
    let InFlight {
        reply, file, buf, ..
    } = op;
    drop(buf);
    drop(file);
    // The caller may have dropped its future (cancellation); the
    // result then has no reader and is discarded.
    let _ = reply.send(result);
}

/// Drains the CQ and advances or completes each op.
fn drain_completions(ring: &mut io_uring::IoUring, pending: &mut PendingOps) {
    loop {
        let next = ring.completion().next();
        let Some(cqe) = next else { break };
        let Some(op) = pending.0.remove(&cqe.user_data()) else {
            continue;
        };
        on_completion(ring, pending, cqe.user_data(), op, cqe.result());
    }
}

/// Handles one CQE for `op`: resubmits the rest of a short write,
/// otherwise finishes the op.
fn on_completion(
    ring: &mut io_uring::IoUring,
    pending: &mut PendingOps,
    id: u64,
    mut op: InFlight,
    res: i32,
) {
    let Ok(n) = usize::try_from(res) else {
        finish(
            op,
            Err(std::io::Error::from_raw_os_error(res.saturating_neg())),
        );
        return;
    };
    let Some(total) = op.buf.as_ref().map(|b| b.as_slice().len()) else {
        finish(op, Ok(0));
        return;
    };
    if n == 0 {
        // No progress: report the short count instead of spinning.
        let done = op.done;
        finish(op, Ok(done));
        return;
    }
    op.done = op.done.saturating_add(n).min(total);
    if op.done < total {
        submit_or_fail(ring, pending, id, op);
    } else {
        finish(op, Ok(total));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// eventfd primitives
// ─────────────────────────────────────────────────────────────────────────────

/// Create a non-blocking eventfd via libc.
fn create_eventfd() -> Result<OwnedFd> {
    // SAFETY: `libc::eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC)` is a
    // safe syscall returning a new fd or -1. We check the result
    // before constructing OwnedFd.
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` is a valid open file descriptor we just
    // received from `eventfd(2)`; OwnedFd::from_raw_fd takes
    // ownership.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Read the eventfd to clear its counter (level-triggered).
fn clear_eventfd(fd: RawFd) {
    let mut buf: u64 = 0;
    // SAFETY: `fd` is a valid eventfd (registered with the ring
    // and wrapped in our AsyncFd). Reading 8 bytes into a
    // properly-aligned `&mut u64` is the standard eventfd
    // clear-pattern. Read errors (EAGAIN) are ignored: a spurious
    // wakeup is not actionable.
    let _ = unsafe {
        libc::read(
            fd,
            &mut buf as *mut u64 as *mut libc::c_void,
            std::mem::size_of::<u64>(),
        )
    };
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Skip if io_uring or eventfd is unavailable on this runner.
    fn ring_or_skip() -> Option<AsyncIoUring> {
        AsyncIoUring::new(8).ok()
    }

    /// A `FileRef` naming an invalid fd; the kernel answers `EBADF`.
    fn bad_file() -> FileRef {
        FileRef::new(Arc::new(()), |_| -1)
    }

    /// Takes the owner task's handle out of `ring`.
    fn take_join(ring: &AsyncIoUring) -> tokio::task::JoinHandle<()> {
        ring.join
            .lock()
            .expect("ring.join mutex poisoned")
            .take()
            .expect("owner task handle already taken")
    }

    /// 0.9.6 hardening: wraps an async test body with a hard
    /// 15-second timeout so a regression hangs in seconds, not
    /// the CI job timeout. Tests that pick their own tighter
    /// timeout keep it.
    async fn with_timeout<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        const TIMEOUT_SECS: u64 = 15;
        match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), fut).await {
            Ok(v) => v,
            Err(_) => panic!(
                "test exceeded {TIMEOUT_SECS}s timeout, likely a hang in the completion driver"
            ),
        }
    }

    #[tokio::test]
    async fn construction_returns_or_skips() {
        with_timeout(async {
            // Either construction succeeded or the runner lacks
            // io_uring; either way it must not panic.
            let _ring = ring_or_skip();
        })
        .await;
    }

    #[tokio::test]
    async fn test_drop_ring_owner_task_exits() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let join = take_join(&ring);
            drop(ring);
            join.await.expect("owner task exits cleanly after drop");
        })
        .await;
    }

    /// Validates the load-bearing invariant from
    /// `.dev/DECISIONS-0.7.0.md` "Critical reminders": a poisoned
    /// handle must answer with an error, never hang.
    #[tokio::test]
    async fn poisoned_flag_short_circuits_submit() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            ring.poisoned.store(true, Ordering::Release);
            let result = ring
                .submit(|reply| Op::Fdatasync {
                    file: bad_file(),
                    reply,
                })
                .await;
            assert!(matches!(result, Err(Error::HandlePoisoned { .. })));
        })
        .await;
    }

    /// Abort the owner task (same drop signature as a panic inside
    /// the loop) and verify a later submit resolves promptly with a
    /// defined error.
    #[tokio::test]
    async fn aborted_owner_task_translates_to_clean_error() {
        let Some(ring) = ring_or_skip() else { return };
        let join = take_join(&ring);
        join.abort();
        let _cancelled = join.await;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ring.submit(|reply| Op::Fdatasync {
                file: bad_file(),
                reply,
            }),
        )
        .await;
        let inner = result.expect("submit hung after owner abort");
        assert!(
            matches!(
                inner,
                Err(Error::CompletionDriverDead) | Err(Error::HandlePoisoned { .. })
            ),
            "expected poisoned/dead error, got {inner:?}"
        );
        assert!(ring.is_poisoned());
    }

    #[tokio::test]
    async fn fdatasync_against_invalid_fd_returns_error_not_hang() {
        let Some(ring) = ring_or_skip() else { return };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ring.submit(|reply| Op::Fdatasync {
                file: bad_file(),
                reply,
            }),
        )
        .await
        .expect("submit on invalid fd hung; driver isn't draining CQ correctly");
        match result {
            Err(Error::Io(e)) => assert_eq!(e.raw_os_error(), Some(libc::EBADF)),
            other => panic!("expected EBADF, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_empty_write_completes_without_sqe() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let n = ring
                .submit(|reply| Op::Write {
                    file: bad_file(),
                    buf: IoBuf::Vec(Vec::new()),
                    offset: 0,
                    reply,
                })
                .await
                .expect("empty write");
            assert_eq!(n, 0);
        })
        .await;
    }

    /// 0.9.6 audit H-10: many in-flight submits racing an owner
    /// abort. Every submitter must resolve to a defined result
    /// within the timeout, never hang.
    #[tokio::test]
    async fn concurrent_submits_resolve_cleanly_on_owner_abort() {
        let Some(ring) = ring_or_skip() else { return };
        let ring = Arc::new(ring);
        const SUBMITTERS: usize = 16;

        let mut handles = Vec::with_capacity(SUBMITTERS);
        for _ in 0..SUBMITTERS {
            let ring = Arc::clone(&ring);
            handles.push(tokio::spawn(async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    ring.submit(|reply| Op::Fdatasync {
                        file: bad_file(),
                        reply,
                    }),
                )
                .await
            }));
        }

        // Give the submitters a moment to enqueue their ops.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let join = take_join(&ring);
        join.abort();
        let _cancelled = join.await;

        for h in handles {
            let outer = h.await.expect("submitter task panicked");
            let inner = outer.expect("submitter timeout, owner abort didn't propagate within 5s");
            match inner {
                // Completed before the abort: the kernel returned
                // EBADF.
                Err(Error::Io(_)) => {}
                // Submitted after the abort, or its reply was
                // dropped by the aborted owner.
                Err(Error::CompletionDriverDead) | Err(Error::HandlePoisoned { .. }) => {}
                other => panic!("unexpected submitter result: {other:?}"),
            }
        }
    }

    /// FS-C2: a write whose future is dropped right after it was
    /// queued must still write the bytes it was given, because the
    /// driver owns the buffer. Before 1.1.1 the SQE pointed into the
    /// caller's buffer, which the cancelled caller freed and reused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cancelled_writes_finish_with_original_bytes() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let path = std::env::temp_dir().join(format!(
                "fsys_driver_cancel_{}_{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let file = Arc::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)
                    .expect("open"),
            );
            const OPS: usize = 64;
            const LEN: usize = 4096;
            for i in 0..OPS {
                let payload = vec![(i % 251) as u8 + 1; LEN];
                let fut = ring.submit(|reply| Op::Write {
                    file: FileRef::new(Arc::clone(&file), |f| f.as_raw_fd()),
                    buf: IoBuf::Vec(payload),
                    offset: (i * LEN) as u64,
                    reply,
                });
                // Poll once (queues the op), then drop the future.
                let _elapsed = tokio::time::timeout(Duration::ZERO, fut).await;
                // Churn the allocator so a freed buffer would be
                // overwritten.
                let scratch = vec![0xEEu8; LEN];
                drop(std::hint::black_box(scratch));
            }
            // Dropping the ring lets the owner finish every queued op
            // and exit; awaiting the task waits for exactly that.
            let join = take_join(&ring);
            drop(ring);
            join.await.expect("owner exits");
            drop(file);
            let bytes = std::fs::read(&path).expect("read back");
            let _cleanup = std::fs::remove_file(&path);
            assert_eq!(bytes.len(), OPS * LEN);
            for i in 0..OPS {
                let want = (i % 251) as u8 + 1;
                let chunk = &bytes[i * LEN..(i + 1) * LEN];
                assert!(
                    chunk.iter().all(|&b| b == want),
                    "op {i}: cancelled write landed with foreign bytes"
                );
            }
        })
        .await;
    }

    /// The fd keep-alive: the caller drops its only handle to the
    /// file while the op is queued. The driver's `FileRef` keeps the
    /// fd open, so the write still reaches the right file.
    #[tokio::test]
    async fn test_file_ref_keeps_fd_open_until_completion() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let path =
                std::env::temp_dir().join(format!("fsys_driver_fileref_{}", std::process::id()));
            let file = Arc::new(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)
                    .expect("open"),
            );
            let file_ref = FileRef::new(Arc::clone(&file), |f| f.as_raw_fd());
            drop(file);
            let n = ring
                .submit(|reply| Op::Write {
                    file: file_ref,
                    buf: IoBuf::Vec(b"kept open".to_vec()),
                    offset: 0,
                    reply,
                })
                .await
                .expect("write through FileRef");
            assert_eq!(n, 9);
            let bytes = std::fs::read(&path).expect("read back");
            let _cleanup = std::fs::remove_file(&path);
            assert_eq!(bytes, b"kept open");
        })
        .await;
    }
}
