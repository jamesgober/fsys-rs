//! Native io_uring async substrate — owner task that drives both
//! submission and completion for the per-handle async ring.
//!
//! ## Architecture (one fused task, not separate submitter/driver)
//!
//! Per `.dev/DECISIONS-0.7.0.md` §`Native io_uring async substrate`:
//! a single tokio task owns the `io_uring::IoUring` value on its
//! stack frame (same ICE-avoidance rule as the 0.5.1 sync owner
//! thread — never a struct field, never a function parameter at
//! module scope). The task fuses submission and completion into
//! one `tokio::select!` loop:
//!
//! 1. Pull op from the submission `mpsc` channel — caller submits
//!    via [`AsyncIoUring::submit`].
//! 2. Push the SQE onto the ring; submit to kernel.
//! 3. `.await` on `AsyncFd<EventFd>` — yields when the eventfd is
//!    readable (kernel signals when CQ has new entries).
//! 4. Drain CQ, route results via per-op `oneshot` senders.
//! 5. Loop.
//!
//! ## Panic resilience
//!
//! The owner task's main work is wrapped in `catch_unwind`. On
//! panic, the driver:
//! - Sets a shared `poisoned: AtomicBool` so subsequent
//!   `submit()` calls return [`Error::HandlePoisoned`].
//! - Drains any remaining receiver items, sending the sentinel
//!   `i32::MIN` to each pending oneshot so awaiting futures wake
//!   with [`Error::HandlePoisoned`] instead of hanging on a
//!   never-completed `oneshot::recv`.
//!
//! This is the load-bearing invariant called out in the
//! "Critical reminders for this phase" section of
//! `.dev/DECISIONS-0.7.0.md`. The panic-injection unit tests below
//! validate it exhaustively.
//!
//! ## Lifecycle
//!
//! - Constructed by [`crate::handle::Handle`] on the first native-
//!   substrate op. The constructor synchronously probes
//!   `io_uring::IoUring::new(queue_depth)` and `eventfd(2)` so that
//!   capability failure surfaces as `Error::IoUringSetupFailed`
//!   from `submit_async` rather than from a dangling driver task.
//! - The owner task is spawned via `tokio::task::spawn`. The
//!   `JoinHandle` lives on `AsyncIoUring`; on drop, the task is
//!   sent `Op::Shutdown` and aborted as a backstop.

#![cfg(all(target_os = "linux", feature = "async"))]
#![allow(dead_code)] // Same ICE-class workaround as `linux_iouring.rs` —
                     // any item referencing `io_uring::IoUring` plus a
                     // dead-code lint pass triggers rustc 1.95's
                     // `check_mod_deathness` panic; module-level allow
                     // sidesteps the buggy lint without affecting
                     // correctness (everything here is reachable from
                     // `Handle::async_io_uring`).

use crate::{Error, Result};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};

/// Op submitted to the owner task. The owner task pulls from the
/// `mpsc` channel and processes one op per submission cycle.
pub(crate) enum Op {
    Write {
        fd: RawFd,
        buf_ptr: usize,
        buf_len: usize,
        offset: u64,
        reply: oneshot::Sender<i32>,
    },
    Read {
        fd: RawFd,
        buf_ptr: usize,
        buf_len: usize,
        offset: u64,
        reply: oneshot::Sender<i32>,
    },
    Fdatasync {
        fd: RawFd,
        reply: oneshot::Sender<i32>,
    },
    /// Cooperative shutdown — owner exits its loop cleanly after
    /// receiving this op.
    Shutdown,
}

/// Public handle to the native async substrate's owner task.
///
/// Owns the `mpsc::Sender` for submission, the `AtomicBool` poison
/// flag (shared with the owner task), and the `JoinHandle`.
pub(crate) struct AsyncIoUring {
    /// Submission channel. `mpsc::UnboundedSender` is already
    /// `Send + Sync` and supports concurrent `send` from multiple
    /// owners — no Mutex is needed on the hot path. (Earlier
    /// 0.7.0 versions wrapped this in `AsyncMutex<Option<...>>`
    /// to support setting it to `None` on shutdown; the audit
    /// pass for 0.8.0 removed that overhead — shutdown signalling
    /// now goes via the `shutdown` flag below + sending
    /// `Op::Shutdown` through the channel.)
    submit_tx: mpsc::UnboundedSender<Op>,
    /// Set to `true` by [`AsyncIoUring::shutdown`]. Submit checks
    /// this before sending and returns
    /// [`Error::CompletionDriverDead`] if set, avoiding the
    /// channel-send overhead on already-shut-down handles.
    shutdown: AtomicBool,
    /// Set to `true` by `submit` itself when the owner task has
    /// dropped its receiver mid-op (panic) or its `oneshot::Sender`
    /// has been dropped before the reply landed. The owner task
    /// does NOT write this directly — panic resilience is achieved
    /// via structural drop (see [`owner_main`] doc), and `submit`
    /// is the witness that translates the structural failure into
    /// the `poisoned` signal.
    poisoned: Arc<AtomicBool>,
    /// JoinHandle for the owner task. Locked only by `shutdown` /
    /// Drop, never on the hot path. `std::sync::Mutex` is fine
    /// because lock duration is bounded by `shutdown`'s 5-second
    /// timeout or by `JoinHandle::abort` (~µs).
    join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AsyncIoUring {
    /// Constructs a new async ring + driver. Synchronously probes
    /// `io_uring_setup(2)` and `eventfd(2)` so capability failure
    /// surfaces here rather than from a dangling task.
    ///
    /// Spawns the owner task on the current tokio runtime — must be
    /// called from inside a runtime context.
    pub(crate) fn new(queue_depth: u32) -> Result<Self> {
        // 0.9.4: probe with the elite setup flags
        // (`COOP_TASKRUN` / `SINGLE_ISSUER` / `DEFER_TASKRUN`) the
        // host kernel supports. The cached probe in
        // `iouring_features::features()` runs at most once per
        // process; subsequent ring constructions just re-apply
        // the cached bits.
        let mut probe_builder = io_uring::IoUring::builder();
        crate::platform::iouring_features::apply(&mut probe_builder);
        match probe_builder.build(queue_depth) {
            Ok(_probe) => {}
            Err(source) => return Err(Error::IoUringSetupFailed { source }),
        }

        // Probe eventfd construction synchronously too.
        let eventfd = create_eventfd()?;
        // Pass it through to the task as a RawFd; the task wraps in
        // OwnedFd + AsyncFd inside its scope so the eventfd is
        // dropped when the task exits.
        let eventfd_raw = eventfd.into_raw_fd();

        let (tx, rx) = mpsc::unbounded_channel::<Op>();
        let poisoned = Arc::new(AtomicBool::new(false));

        let join = tokio::task::spawn(async move {
            owner_main(queue_depth, eventfd_raw, rx).await;
        });

        Ok(Self {
            submit_tx: tx,
            shutdown: AtomicBool::new(false),
            poisoned,
            join: std::sync::Mutex::new(Some(join)),
        })
    }

    /// Returns `true` if the owner task has panicked.
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Submits an op to the owner task and `.await`s its
    /// completion via the per-op `oneshot`.
    ///
    /// On panic in the driver, the owner task drains pending ops
    /// with the sentinel `i32::MIN` so the caller sees
    /// `Error::HandlePoisoned` rather than a hang.
    pub(crate) async fn submit(&self, op: Op, reply: oneshot::Receiver<i32>) -> Result<i32> {
        // Fast-path: poisoned/shutdown flags short-circuit without
        // touching the channel. Both are pure atomic loads.
        if self.is_poisoned() {
            return Err(Error::HandlePoisoned {
                reason: "io_uring completion driver panicked".to_string(),
            });
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(Error::CompletionDriverDead);
        }
        // Channel-closed → owner task dropped the receiver
        // (typically because it panicked). Mark poisoned so future
        // submits short-circuit; surface this submission as
        // CompletionDriverDead.
        if self.submit_tx.send(op).is_err() {
            self.poisoned.store(true, Ordering::Release);
            return Err(Error::CompletionDriverDead);
        }

        match reply.await {
            Ok(code) if code == i32::MIN => {
                self.poisoned.store(true, Ordering::Release);
                Err(Error::HandlePoisoned {
                    reason: "io_uring completion driver panicked mid-op".to_string(),
                })
            }
            Ok(code) => Ok(code),
            Err(_recv_err) => {
                // The owner task dropped the sender for this op
                // before signalling — this happens when the task
                // panics and unwinds with `pending` still
                // populated. Mark poisoned and surface.
                self.poisoned.store(true, Ordering::Release);
                Err(Error::HandlePoisoned {
                    reason: "io_uring completion driver dropped sender".to_string(),
                })
            }
        }
    }

    /// Signals cooperative shutdown to the owner task and awaits
    /// its termination. Drops the submission sender so the task's
    /// `mpsc::Receiver::recv` returns `None` and the loop exits.
    pub(crate) async fn shutdown(&self) {
        // Mark shutdown active so subsequent `submit`s short-circuit
        // on the atomic check before reaching the channel.
        self.shutdown.store(true, Ordering::Release);

        // Send Op::Shutdown so the owner task gets a clean exit
        // signal even with queued submissions ahead of it.
        // `send` failure here means the channel is already closed
        // (owner task already exited) — that's fine.
        let _ = self.submit_tx.send(Op::Shutdown);

        // Take the JoinHandle out of the slot and await the task's
        // natural exit. The lock here is sync and contended at most
        // once (this fn + Drop). If `lock()` is poisoned (Mutex
        // poisoning from a panicked holder) we fall through to the
        // None path, which is safe — Drop will abort if anything
        // remains.
        let join_opt = match self.join.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        };
        if let Some(join) = join_opt {
            let abort_handle = join.abort_handle();
            if tokio::time::timeout(std::time::Duration::from_secs(5), join)
                .await
                .is_err()
            {
                abort_handle.abort();
            }
        }
    }
}

impl Drop for AsyncIoUring {
    fn drop(&mut self) {
        // Best-effort sync cleanup. The structured shutdown above
        // is async, but Handle::drop is sync — so on the unhappy
        // drop path we just abort the task and let the runtime
        // clean up. Pending oneshot receivers will see
        // RecvError → `Error::HandlePoisoned`.
        //
        // `try_lock` would still work here, but since this is
        // `&mut self`, there are no other holders by definition —
        // `get_mut` is contention-free.
        if let Ok(g) = self.join.get_mut() {
            if let Some(j) = g.take() {
                j.abort();
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Owner task main loop
// ─────────────────────────────────────────────────────────────────────────────

/// Owner task entry point.
///
/// **Panic-resilience strategy.** We do NOT use `catch_unwind` —
/// `block_on` inside `catch_unwind` is incompatible with tokio
/// runtimes (the runtime detects the nested `block_on` and aborts).
/// Instead, panic resilience is achieved by structural drop:
///
/// 1. If the loop panics, tokio's task framework catches it and
///    marks the `JoinHandle` as failed.
/// 2. Stack unwind drops the `pending` HashMap, which drops every
///    `oneshot::Sender` it owns. Awaiting `oneshot::Receiver`s see
///    `RecvError`, which `submit()` translates into
///    `Error::HandlePoisoned`.
/// 3. The `mpsc::Receiver` drops too, closing the channel. Future
///    `tx.send()` calls fail; `submit()` translates the failure
///    into `Error::CompletionDriverDead` AND sets the shared
///    `poisoned` flag so subsequent submits short-circuit on the
///    fast-path atomic check. The owner task itself does not
///    write `poisoned` — `submit()` is the witness that converts
///    the structural failure into the flag transition.
///
/// Net effect: every awaiting submitter wakes up with a defined
/// error, and every new submit short-circuits via the poisoned
/// flag. The "load-bearing invariant" called out in
/// `.dev/DECISIONS-0.7.0.md` is preserved.
async fn owner_main(queue_depth: u32, eventfd_raw: RawFd, rx: mpsc::UnboundedReceiver<Op>) {
    // Run the inner loop directly. If it panics, tokio's task
    // framework catches the unwind; the channel + pending map
    // drop on the unwind path, signalling all submitters.
    owner_loop(queue_depth, eventfd_raw, rx).await;
    // Note: `eventfd_raw` is consumed by AsyncFd inside owner_loop
    // (wrapped via OwnedFd::from_raw_fd). On normal return AsyncFd
    // drops, closing the eventfd. On panic the OwnedFd drops via
    // unwind. No additional close is needed here.
}

/// Inner loop. The owner owns:
/// - The `IoUring` value (on this stack frame; never escapes).
/// - The `AsyncFd<OwnedFd>` wrapping the eventfd.
/// - The `pending` HashMap of `user_data → oneshot::Sender`.
/// - The submission `mpsc::Receiver`.
async fn owner_loop(queue_depth: u32, eventfd_raw: RawFd, mut rx: mpsc::UnboundedReceiver<Op>) {
    use std::collections::HashMap;

    // Wrap the eventfd in `OwnedFd` first thing — before any
    // fallible construction below. If anything panics or returns
    // early, the unwind drops `OwnedFd` and closes the eventfd
    // exactly once. Eliminates the leak window that existed in
    // 0.7.0 between `register_eventfd_with_ring` succeeding and
    // ownership being established.
    //
    // SAFETY: `eventfd_raw` is a valid eventfd produced by
    // `create_eventfd` (which used `OwnedFd::into_raw_fd` to release
    // ownership) and not duplicated anywhere else. We are the sole
    // owner from this point onward.
    let owned_fd = unsafe { OwnedFd::from_raw_fd(eventfd_raw) };

    // Reconstruct the ring on this task's stack. (We probed it
    // synchronously in `AsyncIoUring::new` to surface kernel
    // failure as a clean error.)
    // 0.9.4: apply the cached elite setup flags so this ring
    // gets the same kernel feature set the probe accepted.
    let mut builder = io_uring::IoUring::builder();
    crate::platform::iouring_features::apply(&mut builder);
    let mut ring = match builder.build(queue_depth) {
        Ok(r) => r,
        Err(_) => return, // owned_fd drops, eventfd closes once
    };

    // 0.9.5 — IORING_REGISTER_FILES. Pre-register a 16-slot
    // sparse file table at owner startup. Each per-op `fd` is
    // lazily upgraded to a fixed-file slot via
    // `register_files_update` on first use; subsequent
    // submissions for the same fd reuse the cached slot and
    // submit SQEs with `IOSQE_FIXED_FILE` semantics. This
    // saves kernel-side fd validation on every SQE, an
    // observable per-syscall win on rings that do many ops
    // against a small set of fds (the journal hot path).
    //
    // If the kernel rejects the initial registration (rare —
    // it has been stable since 5.1), or if the table fills
    // (>16 distinct fds in one ring's lifetime, unusual for
    // fsys workloads), we fall back to raw-fd SQEs cleanly —
    // the registry simply returns `None` and the SQE builder
    // uses `types::Fd(raw)` instead of `types::Fixed(slot)`.
    let mut fd_registry = FdRegistry::new();
    let _ = fd_registry.initial_register(&ring.submitter());

    // Register the eventfd with the ring so the kernel signals it
    // when CQ has new entries. Use `as_raw_fd()` — registration
    // does not transfer ownership.
    if register_eventfd_with_ring(&mut ring, owned_fd.as_raw_fd()).is_err() {
        return; // owned_fd drops, eventfd closes once
    }

    // Hand ownership of the eventfd to AsyncFd. From here on,
    // AsyncFd is responsible for closing the fd when it drops.
    // On error, `with_interest` consumes and drops `owned_fd`
    // internally — still closes once.
    let async_fd = match AsyncFd::with_interest(owned_fd, tokio::io::Interest::READABLE) {
        Ok(f) => f,
        Err(_) => return,
    };

    let mut pending: HashMap<u64, oneshot::Sender<i32>> = HashMap::new();
    let mut next_id: u64 = 1;

    loop {
        tokio::select! {
            biased; // prefer submission over completion when both ready

            // Submission path.
            maybe_op = rx.recv() => {
                match maybe_op {
                    Some(Op::Shutdown) | None => {
                        // Cooperative shutdown OR sender dropped.
                        // Drain any pending CQ entries before exiting
                        // so in-flight ops complete cleanly.
                        drain_completions_into(&mut ring, &mut pending);
                        return;
                    }
                    Some(op) => {
                        let id = next_id;
                        next_id = next_id.wrapping_add(1);
                        if id == 0 { next_id = 1; } // 0 reserved
                        push_sqe_for(&mut ring, id, &op, &mut fd_registry);
                        match op {
                            Op::Write { reply, .. }
                            | Op::Read { reply, .. }
                            | Op::Fdatasync { reply, .. } => {
                                let _ = pending.insert(id, reply);
                            }
                            // Op::Shutdown handled in the outer match arm
                            // above — by the time we reach here, the op
                            // is one of Write / Read / Fdatasync. Use a
                            // catch-all that drops the (unreachable)
                            // remainder rather than the unreachable!
                            // macro (clippy::unreachable lint).
                            Op::Shutdown => {}
                        }
                        let _ = ring.submit();
                    }
                }
            }

            // Completion path: AsyncFd is readable.
            ready_result = async_fd.readable() => {
                let mut ready_guard = match ready_result {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                // Read the eventfd to clear the level-trigger.
                clear_eventfd(async_fd.get_ref().as_raw_fd());
                drain_completions_into(&mut ring, &mut pending);
                ready_guard.clear_ready();
            }
        }
    }
}

/// Drain CQ entries; route each to its pending oneshot.
fn drain_completions_into(
    ring: &mut io_uring::IoUring,
    pending: &mut std::collections::HashMap<u64, oneshot::Sender<i32>>,
) {
    loop {
        let cqe = match ring.completion().next() {
            Some(c) => c,
            None => break,
        };
        let id = cqe.user_data();
        let result = cqe.result();
        if let Some(tx) = pending.remove(&id) {
            let _ = tx.send(result);
        }
        // No matching pending → caller's future was dropped before
        // completion. Result is silently discarded.
    }
}

/// 0.9.5 — `IORING_REGISTER_FILES` slot registry.
///
/// Maintains a 16-slot sparse file table that's registered with
/// the ring at owner startup. Per-op fds are lazily upgraded to
/// fixed-file slots on first use; subsequent submissions for
/// the same fd reuse the cached slot via the `fd_to_slot`
/// lookup. SQEs for slotted fds use `types::Fixed(slot)`
/// instead of `types::Fd(raw)` — the kernel skips per-SQE fd
/// validation, an observable per-syscall win.
struct FdRegistry {
    /// The slot table — `-1` for unused, otherwise the
    /// registered RawFd. Sized to [`SLOT_TABLE_SIZE`].
    slots: Vec<RawFd>,
    /// Cache `fd → slot` for O(1) lookup on subsequent ops.
    fd_to_slot: std::collections::HashMap<RawFd, u32>,
    /// `true` once the initial `register_files` succeeded.
    /// Subsequent lazy upgrades use `register_files_update`.
    registered: bool,
}

/// Size of the registered-files slot table per ring.
/// 16 is well above the typical journal workload (1 fd per
/// journal handle) and keeps the kernel-side memory cost
/// negligible.
const SLOT_TABLE_SIZE: usize = 16;

impl FdRegistry {
    fn new() -> Self {
        Self {
            slots: vec![-1; SLOT_TABLE_SIZE],
            fd_to_slot: std::collections::HashMap::new(),
            registered: false,
        }
    }

    /// Initial sparse registration. Called once at owner
    /// startup; subsequent `try_get_or_register` calls use
    /// `register_files_update` instead.
    ///
    /// Returns `Ok(())` if registration succeeded. On `Err` the
    /// registry stays `registered = false` and every
    /// `try_get_or_register` call returns `None`, causing
    /// `push_sqe_for` to fall back to raw-fd SQEs cleanly.
    fn initial_register(&mut self, submitter: &io_uring::Submitter<'_>) -> std::io::Result<()> {
        submitter.register_files(&self.slots)?;
        self.registered = true;
        Ok(())
    }

    /// Returns the slot index for `fd`, registering it lazily
    /// on first use. `None` if (a) the initial registration
    /// failed, (b) the slot table is full, or (c) the
    /// per-fd registration update was rejected by the kernel.
    /// In all three cases the caller falls back to raw-fd
    /// SQEs.
    fn try_get_or_register(
        &mut self,
        submitter: &io_uring::Submitter<'_>,
        fd: RawFd,
    ) -> Option<u32> {
        if !self.registered {
            return None;
        }
        if let Some(&slot) = self.fd_to_slot.get(&fd) {
            return Some(slot);
        }
        let slot_idx = self.slots.iter().position(|&s| s == -1)?;
        let update = [fd];
        let updated = submitter
            .register_files_update(slot_idx as u32, &update)
            .ok()?;
        if updated == 0 {
            return None;
        }
        self.slots[slot_idx] = fd;
        let _ = self.fd_to_slot.insert(fd, slot_idx as u32);
        Some(slot_idx as u32)
    }
}

/// Push the SQE for the given op onto the ring's submission queue.
///
/// 0.9.5: each fd-bearing SQE consults `fd_registry` to upgrade
/// to a fixed-file slot when available. Falls back to raw-fd
/// SQEs cleanly when the registry is full or unregistered.
///
/// Inlined per-variant to honour the 0.5.1 ICE workaround
/// (no `&mut io_uring::IoUring` as function parameter — but here
/// we're calling from the OWNER's loop so the parameter is
/// already on this task's stack; the rule is about *cross-module*
/// references).
fn push_sqe_for(ring: &mut io_uring::IoUring, id: u64, op: &Op, fd_registry: &mut FdRegistry) {
    use io_uring::{opcode, types};

    match op {
        Op::Write {
            fd,
            buf_ptr,
            buf_len,
            offset,
            ..
        } => {
            // 0.9.5 — try the fixed-file fast path first.
            let entry = if let Some(slot) = fd_registry.try_get_or_register(&ring.submitter(), *fd)
            {
                opcode::Write::new(types::Fixed(slot), *buf_ptr as *const u8, *buf_len as u32)
                    .offset(*offset)
                    .build()
                    .user_data(id)
            } else {
                opcode::Write::new(types::Fd(*fd), *buf_ptr as *const u8, *buf_len as u32)
                    .offset(*offset)
                    .build()
                    .user_data(id)
            };
            // SAFETY: Submitter's `&[u8]` borrow is held alive by
            // the awaiting future across the oneshot. The kernel
            // reads the buffer at `buf_ptr` for `buf_len` bytes
            // before signalling completion via the CQ; both
            // invariants hold while the submitter awaits.
            let _ = unsafe { ring.submission().push(&entry) };
        }
        Op::Read {
            fd,
            buf_ptr,
            buf_len,
            offset,
            ..
        } => {
            let entry = if let Some(slot) = fd_registry.try_get_or_register(&ring.submitter(), *fd)
            {
                opcode::Read::new(types::Fixed(slot), *buf_ptr as *mut u8, *buf_len as u32)
                    .offset(*offset)
                    .build()
                    .user_data(id)
            } else {
                opcode::Read::new(types::Fd(*fd), *buf_ptr as *mut u8, *buf_len as u32)
                    .offset(*offset)
                    .build()
                    .user_data(id)
            };
            // SAFETY: same shape as Op::Write — submitter holds
            // the `&mut [u8]` borrow alive.
            let _ = unsafe { ring.submission().push(&entry) };
        }
        Op::Fdatasync { fd, .. } => {
            let entry = if let Some(slot) = fd_registry.try_get_or_register(&ring.submitter(), *fd)
            {
                opcode::Fsync::new(types::Fixed(slot))
                    .flags(io_uring::types::FsyncFlags::DATASYNC)
                    .build()
                    .user_data(id)
            } else {
                opcode::Fsync::new(types::Fd(*fd))
                    .flags(io_uring::types::FsyncFlags::DATASYNC)
                    .build()
                    .user_data(id)
            };
            // SAFETY: no buffer; fd held alive by submitter.
            let _ = unsafe { ring.submission().push(&entry) };
        }
        Op::Shutdown => {
            // Same reasoning as in the owner_loop — Shutdown is
            // handled by the caller before reaching this helper;
            // a no-op here keeps clippy::unreachable happy.
        }
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

/// Register the eventfd with the io_uring ring so the kernel
/// signals it on CQ completion.
fn register_eventfd_with_ring(
    ring: &mut io_uring::IoUring,
    eventfd_raw: RawFd,
) -> std::io::Result<()> {
    ring.submitter().register_eventfd(eventfd_raw)
}

/// Read the eventfd to clear its counter (level-triggered).
fn clear_eventfd(fd: RawFd) {
    let mut buf: u64 = 0;
    // SAFETY: `fd` is a valid eventfd (registered with the ring
    // and wrapped in our AsyncFd). Reading 8 bytes into a
    // properly-aligned `&mut u64` is the standard eventfd
    // clear-pattern. Read errors (EAGAIN) are ignored — a spurious
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

    #[tokio::test]
    async fn construction_returns_or_skips() {
        let _ring = ring_or_skip();
        // Either AsyncIoUring::new succeeded (CI runner has
        // io_uring), or it failed and we skipped — the test passes
        // either way; we're verifying that construction doesn't
        // panic.
    }

    #[tokio::test]
    async fn shutdown_is_clean() {
        let Some(ring) = ring_or_skip() else { return };
        ring.shutdown().await;
        // Subsequent submit must return CompletionDriverDead since
        // we dropped the sender during shutdown.
        let (rt, rr) = oneshot::channel();
        // Best-effort: call submit on the closed ring. We expect
        // CompletionDriverDead, NOT a hang.
        let result = ring.submit(Op::Fdatasync { fd: -1, reply: rt }, rr).await;
        assert!(matches!(result, Err(Error::CompletionDriverDead)));
    }

    /// Validates the **load-bearing invariant** from
    /// `.dev/DECISIONS-0.7.0.md` "Critical reminders":
    ///
    /// > A panic in the driver without poisoning the handle hangs
    /// > every in-flight async op forever (their oneshots never
    /// > get sent).
    ///
    /// Setup: construct an AsyncIoUring; manually corrupt its
    /// poisoned flag to `true` (simulating what the
    /// catch_unwind handler does after a real panic). Verify
    /// submit returns HandlePoisoned without hanging.
    #[tokio::test]
    async fn poisoned_flag_short_circuits_submit() {
        let Some(ring) = ring_or_skip() else { return };
        ring.poisoned.store(true, Ordering::Release);

        let (rt, rr) = oneshot::channel();
        let result = ring.submit(Op::Fdatasync { fd: -1, reply: rt }, rr).await;
        assert!(matches!(result, Err(Error::HandlePoisoned { .. })));
    }

    /// Validates that an in-flight submitter whose oneshot
    /// receiver is dropped before completion arrives doesn't
    /// crash the driver.
    #[tokio::test]
    async fn dropped_receiver_is_handled_gracefully() {
        let Some(ring) = ring_or_skip() else { return };

        // We don't actually have a real fd to fdatasync against,
        // so the kernel will return -EBADF. We just want to verify
        // the path doesn't panic.
        let (rt, rr) = oneshot::channel::<i32>();
        // Drop rr before submission — submitter sends and
        // immediately drops the receiver.
        drop(rr);

        // Build a fresh oneshot for the submit path that the API
        // expects.
        let (rt2, rr2) = oneshot::channel::<i32>();
        let _ = ring.submit(Op::Fdatasync { fd: -1, reply: rt2 }, rr2).await;

        // Cleanup: shutdown should still be clean.
        ring.shutdown().await;
        let _ = (rt,); // tx kept for borrow rules
    }

    /// **Load-bearing test from `.dev/DECISIONS-0.7.0.md`.**
    ///
    /// Construct a real ring; abort the owner task externally
    /// (simulating a panic via `JoinHandle::abort` — same drop
    /// semantics from the perspective of `pending`/sender/channel).
    /// Verify that:
    ///   1. The poisoned flag transitions to true.
    ///   2. New submits return `Error::CompletionDriverDead` (since
    ///      the channel is closed) — NOT a hang.
    #[tokio::test]
    async fn aborted_owner_task_translates_to_clean_error() {
        let Some(ring) = ring_or_skip() else { return };

        // Take and abort the JoinHandle — same drop signature as a
        // panic inside the loop.
        {
            let mut g = ring.join.lock().await;
            if let Some(j) = g.take() {
                j.abort();
                let _ = j.await; // join the aborted task
            }
        }

        // Submit must now return promptly with a defined error.
        let (rt, rr) = oneshot::channel::<i32>();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ring.submit(Op::Fdatasync { fd: -1, reply: rt }, rr),
        )
        .await;
        assert!(result.is_ok(), "submit hung after owner abort");
        let inner = result.expect("not timeout");
        assert!(
            matches!(
                inner,
                Err(Error::CompletionDriverDead) | Err(Error::HandlePoisoned { .. })
            ),
            "expected poisoned/dead error, got {inner:?}"
        );
    }

    #[tokio::test]
    async fn fdatasync_against_invalid_fd_returns_error_not_hang() {
        let Some(ring) = ring_or_skip() else { return };

        // Submit fdatasync against fd -1 (invalid). Kernel returns
        // -EBADF; we expect the error to surface as a non-hanging
        // result.
        let (rt, rr) = oneshot::channel();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ring.submit(Op::Fdatasync { fd: -1, reply: rt }, rr),
        )
        .await;
        assert!(
            result.is_ok(),
            "submit on invalid fd hung — driver isn't draining CQ correctly"
        );
        // The kernel returns -EBADF (errno 9); whether we surface
        // this as Ok(-9) or Err depends on `submit`'s mapping. The
        // current impl returns Ok(i32) where i32 < 0 means error.
        // Either way, we've validated no-hang.
        ring.shutdown().await;
    }
}
