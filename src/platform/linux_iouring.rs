//! Linux `io_uring` submission wrapper (owner-thread design).
//!
//! ## rustc 1.95 ICE workaround
//!
//! rustc 1.95.0 panics during the `dead_code` analysis pass on this
//! module:
//!
//! ```text
//! thread 'rustc' panicked at library/core/src/slice/index.rs:1031:55:
//!   slice index starts at 23 but ends at 21
//! query stack during panic:
//! #0 [check_mod_deathness] checking deathness of variables in
//!     module `platform::linux_iouring`
//! ```
//!
//! Empirically the trigger is a combination of `io_uring::IoUring`
//! references plus our specific module structure — bisection ruled
//! out individual factors (channel + spawn alone is fine; a single
//! `&mut io_uring::IoUring` parameter alone reproduces; etc.).
//! Module-level `#![allow(dead_code)]` skips the buggy lint path
//! entirely without affecting correctness — every public item in
//! this module is reachable from `Handle::io_uring_ring`, so there
//! is no real dead code to suppress. See the historical record in
//! `.dev/DECISIONS-0.5.0.md`'s "io_uring blocker" section.
//!
//! ## Design — owner thread instead of `Mutex<IoUring>`
//!
//! `io_uring::IoUring` is `!Sync` (the SQ/CQ rings are SPSC). The
//! natural `Mutex<IoUring>` shape was the original blocker for the
//! 0.5.0 lift; we keep the owner-thread design here because it is
//! a cleaner architectural fit for a !Sync resource and because it
//! generalises to a per-thread sharded design in 0.6.0 without an
//! API break.
//!
//! The `io_uring::IoUring` value lives only on the owner thread's
//! stack frame — never as a struct field, never as a function
//! parameter at module scope. All submission logic is inlined into
//! [`owner_loop`]'s match arms.
//!
//! ## Buffer lifetime
//!
//! [`IoUringRing::write_at`] / [`IoUringRing::read_at`] forward the
//! buffer's raw pointer + length through a bounded
//! `crossbeam_channel`, then **block** on a per-op reply channel.
//! The kernel completes the operation before the owner thread
//! signals reply, and the caller's `&[u8]` / `&mut [u8]` borrow is
//! held alive across the call. This is the standard sync-io_uring
//! contract — the unsafe blocks in [`owner_loop`] document the
//! pre-condition explicitly.
//!
//! ## Failure semantics
//!
//! Construction failure (kernel < 5.1, SECCOMP/AppArmor block, no
//! permission, or owner-thread spawn failure) returns
//! [`Error::IoUringSetupFailed`]. Per locked decision #1 + R-2''' in
//! `.dev/DECISIONS-0.5.0.md`, callers (the `Method::Direct` backend
//! in `crud/file.rs`) catch the error and fall back to `O_DIRECT` +
//! `pwrite` + `fdatasync`. `active_method` is **not** downgraded —
//! the durability contract is identical.

#![cfg(target_os = "linux")]
#![allow(dead_code)]

use crate::{Error, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::os::fd::RawFd;
use std::thread::{self, JoinHandle};

/// Per-handle io_uring submission ring.
///
/// Constructed lazily by [`crate::handle::Handle::io_uring_ring`] on
/// the first Direct-method op when the configured method matches.
/// Idle handles cost zero ring memory and no spawned threads.
pub(crate) struct IoUringRing {
    /// Sender for forwarding operations to the owner thread.
    /// `Option` so [`Drop::drop`] can take it (closing the channel)
    /// before joining the owner thread.
    tx: Option<Sender<Op>>,
    /// `JoinHandle` for the owner thread. Joined after `tx` drop so
    /// the thread observes channel close and exits cleanly.
    join: Option<JoinHandle<()>>,
}

/// Operations the owner thread can execute against the ring.
enum Op {
    Write {
        fd: RawFd,
        buf_ptr: usize,
        buf_len: usize,
        offset: u64,
        reply: Sender<Result<usize>>,
    },
    Read {
        fd: RawFd,
        buf_ptr: usize,
        buf_len: usize,
        offset: u64,
        reply: Sender<Result<usize>>,
    },
    Fdatasync {
        fd: RawFd,
        reply: Sender<Result<()>>,
    },
    /// 0.9.4: linked write + fsync(DATASYNC). The two SQEs are
    /// pushed back-to-back with `IOSQE_IO_LINK` set on the
    /// Write so the kernel executes them as a single chain and
    /// only signals completion of the chain when both have
    /// executed. Halves the durability syscall round-trip vs
    /// submitting two independent SQEs and waiting for each.
    WriteLinkedFsync {
        fd: RawFd,
        buf_ptr: usize,
        buf_len: usize,
        offset: u64,
        reply: Sender<Result<usize>>,
    },
    /// 0.9.6: register a fixed set of buffers with the ring via
    /// `IORING_REGISTER_BUFFERS`. The kernel pins the buffer
    /// pages, hands back slot indices, and subsequent
    /// `Op::WriteFixed` submissions reference the buffer by
    /// slot index rather than re-mapping pages every SQE.
    ///
    /// The `iovs` carry (ptr_as_usize, len) tuples. The reply
    /// is `Result<()>` — the kernel reports success/failure for
    /// the whole batch, and the caller assumes registered-slot
    /// indices `0..N-1` for the N iovs it passed.
    RegisterBuffers {
        iovs: Vec<(usize, usize)>,
        reply: Sender<Result<()>>,
    },
    /// 0.9.6: `IORING_OP_WRITE_FIXED` submission. `buf_idx`
    /// references a previously-registered buffer slot (via
    /// `Op::RegisterBuffers`); `buf_ptr` + `buf_len` must
    /// describe a sub-region within that registered buffer.
    /// The kernel skips per-SQE buffer-page pinning and
    /// page-table lookups — observable per-submission win for
    /// the journal hot path that reuses the LogBuffer's two
    /// AlignedBuf slots thousands of times.
    WriteFixed {
        fd: RawFd,
        buf_idx: u16,
        buf_ptr: usize,
        buf_len: usize,
        offset: u64,
        reply: Sender<Result<usize>>,
    },
}

impl IoUringRing {
    /// Constructs a new ring with `queue_depth` SQ/CQ entries.
    ///
    /// Probes ring construction synchronously on the calling thread
    /// before spawning the owner. If `io_uring_setup(2)` is rejected
    /// (kernel < 5.1, SECCOMP block, AppArmor restriction, container
    /// missing the syscall) we surface that as
    /// [`Error::IoUringSetupFailed`] from this function rather than
    /// from a dangling owner thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IoUringSetupFailed`] when ring construction
    /// or owner-thread spawn fails.
    pub(crate) fn new(queue_depth: u32, sqpoll_idle_ms: Option<u32>) -> Result<Self> {
        // Probe synchronously. Drop the probe ring before spawning;
        // reconstruction in the owner thread is microsecond-scale,
        // and channel transport of `IoUring` is awkward (it's
        // `!Sync`, and the cleaner pattern is to keep all
        // `IoUring`-typed values out of struct fields).
        // 0.9.4: probe builds with the elite setup flags
        // (`COOP_TASKRUN` / `SINGLE_ISSUER` / `DEFER_TASKRUN`) that
        // the host kernel supports. The probe in
        // `iouring_features::features()` happens at most once per
        // process; ring construction here just calls
        // `apply(&mut builder)` to set the cached bits.
        //
        // 0.9.7 SQPOLL — when the caller opts in via
        // `Builder::sqpoll(idle_ms)`, enable `IORING_SETUP_SQPOLL`
        // which spawns a kernel-side polling thread to drain the
        // submission queue without requiring `io_uring_enter`
        // syscalls. May fail with `EPERM` on kernels < 5.13 without
        // `CAP_SYS_NICE`, in sandboxed containers, or under restrictive
        // SECCOMP. On setup failure we bubble the error up as
        // `IoUringSetupFailed` — the caller's `iouring_slot` slot
        // then flips to `Disabled` and the Direct path falls back
        // to non-SQPOLL pwrite, same contract as for any other
        // io_uring setup failure.
        let mut probe_builder = io_uring::IoUring::builder();
        super::iouring_features::apply(&mut probe_builder, super::iouring_features::RingMode::Sync);
        if let Some(idle_ms) = sqpoll_idle_ms {
            probe_builder.setup_sqpoll(idle_ms);
        }
        match probe_builder.build(queue_depth) {
            Ok(_probe) => {}
            Err(source) => return Err(Error::IoUringSetupFailed { source }),
        }

        let cap = (queue_depth as usize).max(1).saturating_mul(2);
        let (tx, rx) = bounded::<Op>(cap);

        let join = thread::Builder::new()
            .name("fsys-iouring".to_string())
            .spawn(move || {
                owner_loop(queue_depth, rx, sqpoll_idle_ms);
            })
            .map_err(|source| Error::IoUringSetupFailed { source })?;

        Ok(Self {
            tx: Some(tx),
            join: Some(join),
        })
    }

    /// Submits a `Write` SQE for `buf` at `offset` on `fd` and waits
    /// for completion.
    ///
    /// The caller's `&[u8]` borrow is held alive across the
    /// blocking reply receive — the owner thread reads the buffer
    /// and signals completion before this method returns.
    pub(crate) fn write_at(&self, fd: RawFd, buf: &[u8], offset: u64) -> Result<usize> {
        let (rt, rr) = bounded::<Result<usize>>(1);
        let buf_ptr = buf.as_ptr() as usize;
        let buf_len = buf.len();
        self.send(Op::Write {
            fd,
            buf_ptr,
            buf_len,
            offset,
            reply: rt,
        })?;
        rr.recv().map_err(|_| owner_dead())?
    }

    /// Submits a `Read` SQE filling `buf` from `offset` on `fd`.
    pub(crate) fn read_at(&self, fd: RawFd, buf: &mut [u8], offset: u64) -> Result<usize> {
        let (rt, rr) = bounded::<Result<usize>>(1);
        let buf_ptr = buf.as_mut_ptr() as usize;
        let buf_len = buf.len();
        self.send(Op::Read {
            fd,
            buf_ptr,
            buf_len,
            offset,
            reply: rt,
        })?;
        rr.recv().map_err(|_| owner_dead())?
    }

    /// Submits an `Fsync(DATASYNC)` SQE on `fd`. Equivalent to
    /// `fdatasync(2)` for durability.
    pub(crate) fn fdatasync(&self, fd: RawFd) -> Result<()> {
        let (rt, rr) = bounded::<Result<()>>(1);
        self.send(Op::Fdatasync { fd, reply: rt })?;
        rr.recv().map_err(|_| owner_dead())?
    }

    /// 0.9.4: Submits a **linked** `Write` + `Fsync(DATASYNC)`
    /// pair against `fd` and returns once both have executed.
    ///
    /// The two SQEs are pushed back-to-back with the `Write`
    /// carrying `IOSQE_IO_LINK`; the kernel executes them as a
    /// single chain and only delivers completions once both
    /// have run. Equivalent to `write_at(fd, buf, offset)`
    /// followed by `fdatasync(fd)`, but with **half** the
    /// `io_uring_enter(2)` round-trips and one merged
    /// kernel-side completion-processing pass.
    ///
    /// Returns the number of bytes written (the `Write`'s
    /// CQE result). The fsync's success/failure is reported as
    /// part of the chain — on fsync failure, the entire call
    /// returns an error and the caller MUST assume the fsync
    /// did not durably commit the write.
    ///
    /// The caller's `&[u8]` borrow is held alive across the
    /// blocking reply receive (same contract as
    /// [`Self::write_at`]).
    pub(crate) fn write_at_linked_fsync(
        &self,
        fd: RawFd,
        buf: &[u8],
        offset: u64,
    ) -> Result<usize> {
        let (rt, rr) = bounded::<Result<usize>>(1);
        let buf_ptr = buf.as_ptr() as usize;
        let buf_len = buf.len();
        self.send(Op::WriteLinkedFsync {
            fd,
            buf_ptr,
            buf_len,
            offset,
            reply: rt,
        })?;
        rr.recv().map_err(|_| owner_dead())?
    }

    /// 0.9.6 — Register a fixed set of buffers with the ring.
    ///
    /// Each `(ptr, len)` tuple in `iovs` becomes a registered
    /// buffer slot at index `0..iovs.len()`. The caller is
    /// responsible for keeping the underlying memory alive
    /// (un-moved, not freed) for the lifetime of the ring —
    /// io_uring pins the pages but doesn't take ownership.
    ///
    /// Slot indices `0..iovs.len()` are then usable as the
    /// `buf_idx` argument to [`Self::write_at_fixed`].
    ///
    /// Returns `Err` on registration failure (kernel rejection,
    /// privilege denial, out-of-resource). On error, no slots
    /// are partially registered — the call is atomic.
    pub(crate) fn register_buffers(&self, iovs: &[(usize, usize)]) -> Result<()> {
        let (rt, rr) = bounded::<Result<()>>(1);
        self.send(Op::RegisterBuffers {
            iovs: iovs.to_vec(),
            reply: rt,
        })?;
        rr.recv().map_err(|_| owner_dead())?
    }

    /// 0.9.6 — Submit an `IORING_OP_WRITE_FIXED` SQE.
    ///
    /// `buf_idx` references a slot previously registered via
    /// [`Self::register_buffers`]. `buf_ptr` + `buf_len` describe
    /// a sub-region within that registered buffer — the kernel
    /// validates that the region fits within the registered
    /// slot. Saves the per-SQE page-pinning cost of `Op::Write`
    /// — the buffer pages were pinned once at registration time.
    pub(crate) fn write_at_fixed(
        &self,
        fd: RawFd,
        buf_idx: u16,
        buf: &[u8],
        offset: u64,
    ) -> Result<usize> {
        let (rt, rr) = bounded::<Result<usize>>(1);
        let buf_ptr = buf.as_ptr() as usize;
        let buf_len = buf.len();
        self.send(Op::WriteFixed {
            fd,
            buf_idx,
            buf_ptr,
            buf_len,
            offset,
            reply: rt,
        })?;
        rr.recv().map_err(|_| owner_dead())?
    }

    fn send(&self, op: Op) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or_else(owner_dead)?
            .send(op)
            .map_err(|_| owner_dead())
    }
}

impl Drop for IoUringRing {
    fn drop(&mut self) {
        // Drop tx so the owner thread observes channel close on its
        // next `rx.recv()` and exits. Then join.
        drop(self.tx.take());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Owner-thread main loop.
///
/// All `io_uring::IoUring` interaction lives here. The mutable ring
/// is **never** passed as a function parameter to a helper — that
/// shape triggers the rustc 1.95 `check_mod_deathness` ICE class
/// (see module docs). Inlining the submit/poll logic per opcode is
/// the workaround.
fn owner_loop(queue_depth: u32, rx: Receiver<Op>, sqpoll_idle_ms: Option<u32>) {
    // 0.9.4: build with the same elite setup flags the
    // `IoUringRing::new` probe accepted. `iouring_features::apply`
    // reads the process-cached probe result, so this is the same
    // flag set the probe succeeded with — no second kernel probe
    // happens here.
    // 0.9.7 SQPOLL: re-apply the same SQPOLL toggle the probe in
    // `IoUringRing::new` succeeded with — the probe ring was
    // dropped before this thread spawned, so we re-build with
    // the identical setup here.
    let mut builder = io_uring::IoUring::builder();
    super::iouring_features::apply(&mut builder, super::iouring_features::RingMode::Sync);
    if let Some(idle_ms) = sqpoll_idle_ms {
        builder.setup_sqpoll(idle_ms);
    }
    let mut ring = match builder.build(queue_depth) {
        Ok(r) => r,
        // The probe in `IoUringRing::new` already succeeded; if
        // reconstruction fails here it's a transient kernel issue.
        // The thread exits, and all subsequent submitter sends will
        // see channel closed and surface the failure to the caller.
        Err(_) => return,
    };

    // 0.9.5: `IORING_REGISTER_FILES`. Pre-register a 16-slot sparse
    // file table at owner startup. Each per-op `fd` is lazily upgraded
    // to a fixed-file slot via `register_files_update` on first use;
    // subsequent submissions for the same fd reuse the cached slot
    // and submit SQEs with `IOSQE_FIXED_FILE` semantics
    // (`io_uring::types::Fixed`). This saves kernel-side fd
    // validation on every SQE — a real per-syscall win for rings
    // that do many ops against a small set of fds (the Direct-method
    // journal hot path).
    //
    // 0.9.6 history: this `initial_register` call was temporarily
    // disabled during the async-substrate hang investigation because
    // an early diagnosis blamed `IORING_REGISTER_FILES`. The real
    // root cause turned out to be `IORING_SETUP_DEFER_TASKRUN` +
    // `IORING_SETUP_SINGLE_ISSUER` interacting with the async
    // substrate's eventfd-driven loop and tokio's multi_thread
    // work-stealing — both now correctly excluded via
    // `RingMode::Async`. The sync ring (this owner_loop) was never
    // the cause; its dedicated `std::thread::spawn` thread satisfies
    // SINGLE_ISSUER and its `submit_and_wait(n)` satisfies
    // DEFER_TASKRUN.
    //
    // 0.9.7 restoration: `initial_register` is back, backed by
    // explicit slot-upgrade + table-full-fallback test coverage in
    // this module (`writes_across_many_distinct_fds_complete_correctly`
    // + `repeated_writes_on_same_fd_round_trip`). The registration
    // is a single syscall on owner startup; on failure (rare —
    // kernel < 5.1, sandbox block, container missing the syscall)
    // the registry stays `registered = false` and every
    // `try_get_or_register` returns `None` → SQEs fall back to
    // `io_uring::types::Fd(raw)` cleanly.
    let mut fd_registry = FdRegistry::new();
    let _ = fd_registry.initial_register(&ring.submitter());

    while let Ok(op) = rx.recv() {
        match op {
            Op::Write {
                fd,
                buf_ptr,
                buf_len,
                offset,
                reply,
            } => {
                // 0.9.5 — try the fixed-file fast path first.
                let entry =
                    if let Some(slot) = fd_registry.try_get_or_register(&ring.submitter(), fd) {
                        io_uring::opcode::Write::new(
                            io_uring::types::Fixed(slot),
                            buf_ptr as *const u8,
                            buf_len as u32,
                        )
                        .offset(offset)
                        .build()
                    } else {
                        io_uring::opcode::Write::new(
                            io_uring::types::Fd(fd),
                            buf_ptr as *const u8,
                            buf_len as u32,
                        )
                        .offset(offset)
                        .build()
                    };
                // SAFETY: The submitter (`IoUringRing::write_at`)
                // is blocked on `reply.recv()` until we send the
                // result, holding the caller's `&[u8]` borrow alive
                // for the duration of this submission. The kernel
                // reads `buf_len` bytes at `buf_ptr`; both
                // invariants hold while the submitter waits.
                let push = unsafe { ring.submission().push(&entry) };
                if push.is_err() {
                    let _ = reply.send(Err(io_err("io_uring submission queue full")));
                    continue;
                }
                let result = match ring.submit_and_wait(1) {
                    Ok(_) => match ring.completion().next() {
                        Some(c) if c.result() < 0 => {
                            Err(Error::Io(std::io::Error::from_raw_os_error(-c.result())))
                        }
                        Some(c) => Ok(c.result() as usize),
                        None => Err(io_err("io_uring completion queue empty")),
                    },
                    Err(e) => Err(Error::Io(e)),
                };
                let _ = reply.send(result);
            }

            Op::Read {
                fd,
                buf_ptr,
                buf_len,
                offset,
                reply,
            } => {
                // 0.9.5 — fixed-file fast path.
                let entry =
                    if let Some(slot) = fd_registry.try_get_or_register(&ring.submitter(), fd) {
                        io_uring::opcode::Read::new(
                            io_uring::types::Fixed(slot),
                            buf_ptr as *mut u8,
                            buf_len as u32,
                        )
                        .offset(offset)
                        .build()
                    } else {
                        io_uring::opcode::Read::new(
                            io_uring::types::Fd(fd),
                            buf_ptr as *mut u8,
                            buf_len as u32,
                        )
                        .offset(offset)
                        .build()
                    };
                // SAFETY: same shape as `Op::Write` — submitter
                // holds the `&mut [u8]` borrow alive across the
                // blocking reply receive.
                let push = unsafe { ring.submission().push(&entry) };
                if push.is_err() {
                    let _ = reply.send(Err(io_err("io_uring submission queue full")));
                    continue;
                }
                let result = match ring.submit_and_wait(1) {
                    Ok(_) => match ring.completion().next() {
                        Some(c) if c.result() < 0 => {
                            Err(Error::Io(std::io::Error::from_raw_os_error(-c.result())))
                        }
                        Some(c) => Ok(c.result() as usize),
                        None => Err(io_err("io_uring completion queue empty")),
                    },
                    Err(e) => Err(Error::Io(e)),
                };
                let _ = reply.send(result);
            }

            Op::Fdatasync { fd, reply } => {
                // 0.9.5 — fixed-file fast path.
                let entry =
                    if let Some(slot) = fd_registry.try_get_or_register(&ring.submitter(), fd) {
                        io_uring::opcode::Fsync::new(io_uring::types::Fixed(slot))
                            .flags(io_uring::types::FsyncFlags::DATASYNC)
                            .build()
                    } else {
                        io_uring::opcode::Fsync::new(io_uring::types::Fd(fd))
                            .flags(io_uring::types::FsyncFlags::DATASYNC)
                            .build()
                    };
                // SAFETY: no buffer; the fd is alive in the
                // submitter (file is held open there) for the
                // duration of this submission.
                let push = unsafe { ring.submission().push(&entry) };
                if push.is_err() {
                    let _ = reply.send(Err(io_err("io_uring submission queue full")));
                    continue;
                }
                let result = match ring.submit_and_wait(1) {
                    Ok(_) => match ring.completion().next() {
                        Some(c) if c.result() < 0 => {
                            Err(Error::Io(std::io::Error::from_raw_os_error(-c.result())))
                        }
                        Some(_) => Ok(()),
                        None => Err(io_err("io_uring completion queue empty")),
                    },
                    Err(e) => Err(Error::Io(e)),
                };
                let _ = reply.send(result);
            }

            Op::WriteLinkedFsync {
                fd,
                buf_ptr,
                buf_len,
                offset,
                reply,
            } => {
                // 0.9.4: linked Write + Fsync(DATASYNC). The
                // Write SQE carries IOSQE_IO_LINK so the
                // kernel queues the following Fsync to run
                // only after the Write completes successfully.
                // We submit both SQEs and wait for both CQEs;
                // the Write's byte count is the reported result.
                //
                // 0.9.5 — both SQEs use the fixed-file slot
                // when available. The slot is resolved once
                // and used for both; falling back to raw fd
                // on the same op if the slot table is full.
                let (write_entry, fsync_entry) =
                    if let Some(slot) = fd_registry.try_get_or_register(&ring.submitter(), fd) {
                        (
                            io_uring::opcode::Write::new(
                                io_uring::types::Fixed(slot),
                                buf_ptr as *const u8,
                                buf_len as u32,
                            )
                            .offset(offset)
                            .build()
                            .flags(io_uring::squeue::Flags::IO_LINK),
                            io_uring::opcode::Fsync::new(io_uring::types::Fixed(slot))
                                .flags(io_uring::types::FsyncFlags::DATASYNC)
                                .build(),
                        )
                    } else {
                        (
                            io_uring::opcode::Write::new(
                                io_uring::types::Fd(fd),
                                buf_ptr as *const u8,
                                buf_len as u32,
                            )
                            .offset(offset)
                            .build()
                            .flags(io_uring::squeue::Flags::IO_LINK),
                            io_uring::opcode::Fsync::new(io_uring::types::Fd(fd))
                                .flags(io_uring::types::FsyncFlags::DATASYNC)
                                .build(),
                        )
                    };
                // SAFETY: submitter blocks on `reply.recv()`
                // holding the caller's `&[u8]` borrow alive
                // for the duration of this submission; the
                // kernel reads `buf_len` bytes at `buf_ptr`.
                // Both invariants hold while the submitter
                // waits. Pushing two SQEs is atomic per the
                // io-uring crate's `SubmissionQueue::push`
                // contract — we hold the queue across both
                // pushes without yielding.
                let push_result = unsafe {
                    let mut sq = ring.submission();
                    sq.push(&write_entry).and_then(|()| sq.push(&fsync_entry))
                };
                if push_result.is_err() {
                    let _ = reply.send(Err(io_err(
                        "io_uring submission queue full (linked write+fsync)",
                    )));
                    continue;
                }
                // Wait for BOTH completions — submit_and_wait(2).
                let result = match ring.submit_and_wait(2) {
                    Ok(_) => {
                        // Drain both CQEs. The completion order
                        // is the submission order (write first,
                        // fsync second) when the chain succeeds;
                        // if the write fails the fsync's CQE
                        // carries -ECANCELED. Either way, we
                        // need both before reporting.
                        let cqe1 = ring.completion().next();
                        let cqe2 = ring.completion().next();
                        match (cqe1, cqe2) {
                            (Some(w), Some(f)) => {
                                if w.result() < 0 {
                                    Err(Error::Io(std::io::Error::from_raw_os_error(-w.result())))
                                } else if f.result() < 0 {
                                    // Write succeeded but fsync
                                    // failed — the caller MUST
                                    // treat the write as not
                                    // durable. Surface the fsync
                                    // error.
                                    Err(Error::Io(std::io::Error::from_raw_os_error(-f.result())))
                                } else {
                                    Ok(w.result() as usize)
                                }
                            }
                            _ => Err(io_err(
                                "io_uring completion queue short on linked write+fsync",
                            )),
                        }
                    }
                    Err(e) => Err(Error::Io(e)),
                };
                let _ = reply.send(result);
            }

            Op::RegisterBuffers { iovs, reply } => {
                // 0.9.6 — IORING_REGISTER_BUFFERS. Pin the
                // caller's buffer ranges in the kernel's
                // page-table so subsequent `WriteFixed` SQEs
                // skip the per-submission page-pinning hop.
                let iovec_array: Vec<libc::iovec> = iovs
                    .iter()
                    .map(|(p, l)| libc::iovec {
                        iov_base: *p as *mut libc::c_void,
                        iov_len: *l,
                    })
                    .collect();
                // SAFETY: the caller (via the public
                // `register_buffers` method) is responsible for
                // keeping the underlying memory alive for the
                // lifetime of the ring. The kernel reads
                // `iovec_array.len()` `iovec` structs, validates
                // the ranges, and pins the pages. The local
                // `iovec_array` lives across the syscall — the
                // kernel only needs the iovec descriptors during
                // the call, not after.
                let result =
                    unsafe { ring.submitter().register_buffers(&iovec_array) }.map_err(Error::Io);
                let _ = reply.send(result);
            }

            Op::WriteFixed {
                fd,
                buf_idx,
                buf_ptr,
                buf_len,
                offset,
                reply,
            } => {
                // 0.9.6 — IORING_OP_WRITE_FIXED. Uses a
                // previously-registered buffer slot; the kernel
                // skips per-SQE page pinning. Tries the
                // fixed-file slot for `fd` too — if the
                // FdRegistry has a slot, double-Fixed win.
                let entry =
                    if let Some(slot) = fd_registry.try_get_or_register(&ring.submitter(), fd) {
                        io_uring::opcode::WriteFixed::new(
                            io_uring::types::Fixed(slot),
                            buf_ptr as *const u8,
                            buf_len as u32,
                            buf_idx,
                        )
                        .offset(offset)
                        .build()
                    } else {
                        io_uring::opcode::WriteFixed::new(
                            io_uring::types::Fd(fd),
                            buf_ptr as *const u8,
                            buf_len as u32,
                            buf_idx,
                        )
                        .offset(offset)
                        .build()
                    };
                // SAFETY: the registered buffer is owned + kept
                // alive by the caller (LogBuffer holds the
                // AlignedBuf for its entire lifetime, longer
                // than this ring). `buf_ptr` + `buf_len`
                // describe a sub-region of the registered slot
                // at `buf_idx`; the kernel validates the range.
                let push = unsafe { ring.submission().push(&entry) };
                if push.is_err() {
                    let _ = reply.send(Err(io_err("io_uring submission queue full (WriteFixed)")));
                    continue;
                }
                let result = match ring.submit_and_wait(1) {
                    Ok(_) => match ring.completion().next() {
                        Some(c) if c.result() < 0 => {
                            Err(Error::Io(std::io::Error::from_raw_os_error(-c.result())))
                        }
                        Some(c) => Ok(c.result() as usize),
                        None => Err(io_err("io_uring completion queue empty (WriteFixed)")),
                    },
                    Err(e) => Err(Error::Io(e)),
                };
                let _ = reply.send(result);
            }
        }
    }
}

fn io_err(msg: &'static str) -> Error {
    Error::Io(std::io::Error::other(msg))
}

fn owner_dead() -> Error {
    Error::Io(std::io::Error::other("io_uring owner thread terminated"))
}

// ─────────────────────────────────────────────────────────────────────────────
// 0.9.5 — `IORING_REGISTER_FILES` slot registry (owner-thread local)
// ─────────────────────────────────────────────────────────────────────────────

/// Maintains a 16-slot sparse file table that's registered with
/// the ring at owner startup. Per-op fds are lazily upgraded to
/// fixed-file slots on first use; subsequent submissions for
/// the same fd reuse the cached slot via the `fd_to_slot`
/// lookup. SQEs for slotted fds use `types::Fixed(slot)` instead
/// of `types::Fd(raw)` — the kernel skips per-SQE fd validation,
/// an observable per-syscall win on the Direct-method journal
/// hot path that reuses the same fd for thousands of writes.
///
/// This is the **synchronous** ring's counterpart to the
/// `FdRegistry` in `async_io::completion_driver` — same shape,
/// same fallback semantics. Two structs (no shared helper)
/// because rustc 1.95's `check_mod_deathness` ICE class
/// triggers on cross-module references involving `&mut
/// io_uring::IoUring`; duplicating the type is the workaround
/// that keeps both modules building cleanly.
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
    /// `try_get_or_register` call returns `None`, causing each
    /// match arm to fall back to raw-fd SQEs cleanly.
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

// ─────────────────────────────────────────────────────────────────────────────
// NVMe passthrough capability detection + flush
// ─────────────────────────────────────────────────────────────────────────────
//
// The 0.6.0 NVMe passthrough flush path uses the legacy
// `NVME_IOCTL_IO_CMD` ioctl rather than `IORING_OP_URING_CMD`. The
// ioctl is synchronous (no io_uring submission), but FLUSH is a
// single-command op whose latency is dominated by the device's
// flush time (~50–100 µs on consumer NVMe), not syscall overhead —
// io_uring submission would add complexity (Entry128 SQEs, ring
// reconstruction, kernel ≥ 5.19 requirement) for zero measurable
// gain on this specific opcode. Filed as refinement R-1 in
// `.dev/DECISIONS-0.6.0.md`.

/// Result of resolving an arbitrary fd to its underlying NVMe
/// character device for passthrough commands.
pub(crate) struct NvmeAccess {
    /// Open file handle on `/dev/nvmeX` (the character device).
    /// Owned by this struct; closed on drop.
    pub(crate) char_dev: std::fs::File,
    /// NVMe namespace ID. `1` for typical single-namespace consumer
    /// drives; we extract it from `/sys/block/.../nsid` when
    /// possible, defaulting to `1` otherwise.
    pub(crate) nsid: u32,
}

/// Probes whether NVMe passthrough flush is available for `fd`.
///
/// Returns `Some(NvmeAccess)` when:
/// 1. `FSYS_DISABLE_NVME_PASSTHROUGH` env override is **not** set
///    (locked decision D-11 in `.dev/DECISIONS-0.6.0.md`).
/// 2. The block device backing `fd` is an NVMe drive.
/// 3. `/dev/nvmeX` (the character device) opens successfully with
///    `O_RDWR` — i.e. the calling process has the privilege to send
///    raw NVMe commands (typically `CAP_SYS_ADMIN` or membership in
///    the `disk` group).
///
/// Returns `None` on any failure. The caller's [`Method::Direct`]
/// path falls back to `fdatasync` on Linux / `WRITE_THROUGH` on
/// Windows per locked decision D-2.
pub(crate) fn nvme_flush_capable(fd: RawFd) -> Option<NvmeAccess> {
    // 1. Env override (testing aid).
    if std::env::var_os("FSYS_DISABLE_NVME_PASSTHROUGH").is_some() {
        return None;
    }

    // 2. Resolve fd → block device → NVMe character device.
    let nvme_dev = nvme_char_device_for(fd)?;
    let nsid = nvme_namespace_id_for(fd).unwrap_or(1);

    // 3. Open the character device. EACCES here is the privilege
    //    boundary we care about — return None for the silent-
    //    fallback path.
    let char_dev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&nvme_dev)
        .ok()?;

    Some(NvmeAccess { char_dev, nsid })
}

/// 0.9.4 — Issues an NVMe Identify Namespace (admin opcode 0x06,
/// CNS=0x00) command via `NVME_IOCTL_ADMIN_CMD` and returns the
/// 4096-byte response buffer. Used by the drive probe to extract
/// the namespace's atomic-write guarantees (NAWUN, NAWUPF, NACWU)
/// which downstream callers consult via
/// [`crate::Handle::atomic_write_unit`].
///
/// # Errors
///
/// Returns [`Error::Io`] wrapping `EACCES` / `EPERM` (privilege
/// denied), `EINVAL` (kernel rejected the ioctl), or the NVMe
/// status code if the controller rejected the command. Probe
/// callers treat any error as "atomic-write unit unknown" and
/// leave the relevant `DriveInfo` fields at `None`.
pub(crate) fn nvme_identify_namespace(nvme_fd: RawFd, nsid: u32) -> Result<[u8; 4096]> {
    #[repr(C)]
    #[derive(Default)]
    struct NvmePassthruCmd {
        opcode: u8,
        flags: u8,
        rsvd1: u16,
        nsid: u32,
        cdw2: u32,
        cdw3: u32,
        metadata: u64,
        addr: u64,
        metadata_len: u32,
        data_len: u32,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
        cdw13: u32,
        cdw14: u32,
        cdw15: u32,
        timeout_ms: u32,
        result: u32,
    }

    // NVME_IOCTL_ADMIN_CMD = _IOWR('N', 0x41, struct nvme_passthru_cmd)
    //   dir=3 (RW) << 30 | size=64 << 16 | 'N' (0x4e) << 8 | nr=0x41
    //   = 0xc040_4e41.
    const NVME_IOCTL_ADMIN_CMD: libc::c_ulong = 0xc040_4e41;
    // NVMe admin opcode: IDENTIFY.
    const OPC_IDENTIFY: u8 = 0x06;
    // CDW10[0..8] = CNS (Controller or Namespace Structure).
    // CNS = 0x00 → Identify Namespace structure for the namespace
    // specified in the NSID field.
    const CNS_NAMESPACE: u32 = 0x0000_0000;
    const ID_BUF_LEN: usize = 4096;

    // Identify response is 4096 bytes; on most kernels the kernel
    // requires the user buffer to be at least 4-byte-aligned. We
    // own a stack array (`[u8; 4096]`) which is 1-byte aligned
    // by default; if any kernel rejects it we'd have to bounce
    // through a heap allocation. So far no platform reference
    // documents a >4-byte requirement for the ADMIN_CMD path.
    let mut buf = [0u8; ID_BUF_LEN];

    let mut cmd = NvmePassthruCmd {
        opcode: OPC_IDENTIFY,
        nsid,
        addr: buf.as_mut_ptr() as u64,
        data_len: ID_BUF_LEN as u32,
        cdw10: CNS_NAMESPACE,
        ..Default::default()
    };

    // SAFETY: `nvme_fd` is owned by the caller for the duration
    // of this synchronous call. `&mut cmd` points to a
    // stack-allocated `NvmePassthruCmd` matching the kernel's
    // expected size. The kernel writes up to `data_len` bytes to
    // `cmd.addr` (our `buf`), which is alive on this stack
    // frame for the duration of the syscall. `ioctl` returns -1
    // on error; we surface `errno` via `last_os_error`.
    let rc = unsafe { libc::ioctl(nvme_fd, NVME_IOCTL_ADMIN_CMD, &mut cmd) };
    if rc < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    // The kernel sets `cmd.result` to the NVMe completion status;
    // 0 means success. Non-zero means the controller rejected the
    // command (e.g. command not supported, namespace inactive).
    if cmd.result != 0 {
        return Err(Error::Io(std::io::Error::other(format!(
            "NVMe Identify Namespace returned status 0x{:x}",
            cmd.result
        ))));
    }
    Ok(buf)
}

/// 0.9.4 — Parses the **NAWUN** and **NAWUPF** fields from a
/// 4096-byte NVMe Identify Namespace response.
///
/// Returns `(nawun_lba, nawupf_lba)` — each as a count of
/// **logical blocks**, **0-based** per the NVMe spec. A value of
/// `Some(0)` means "atomic for one logical block" (the base
/// guarantee); a value of `Some(N)` means "atomic for `N + 1`
/// logical blocks". `None` is returned when the field is the
/// NVMe sentinel `0xFFFF` (unsupported) — only an explicit
/// guarantee should be reported to callers.
///
/// NAWUN (bytes 74-75) is the atomic-write guarantee in normal
/// operation; NAWUPF (bytes 76-77) is the atomic-write
/// guarantee under power-fail. NAWUPF is the load-bearing one
/// for crash-safe atomic writes — it's what
/// [`crate::Handle::atomic_write_unit`] exposes (converted to
/// bytes).
pub(crate) fn parse_nawun_nawupf(id_buf: &[u8; 4096]) -> (Option<u32>, Option<u32>) {
    // Both fields are 16-bit little-endian.
    let nawun = u16::from_le_bytes([id_buf[74], id_buf[75]]);
    let nawupf = u16::from_le_bytes([id_buf[76], id_buf[77]]);
    let cvt = |v: u16| -> Option<u32> {
        if v == u16::MAX {
            None
        } else {
            Some(v as u32)
        }
    };
    (cvt(nawun), cvt(nawupf))
}

/// Issues an NVMe FLUSH (opcode 0x00) on `nvme_fd` for namespace
/// `nsid` via the legacy `NVME_IOCTL_IO_CMD` ioctl.
///
/// This is synchronous from the caller's perspective — the kernel
/// submits the command to the controller, waits for completion, and
/// returns the status. On capable hardware with sufficient
/// privileges, latency is dominated by the device's volatile-cache
/// flush time (~50–100 µs on consumer NVMe).
///
/// # Errors
///
/// Returns [`Error::Io`] wrapping the underlying `EACCES`, `EPERM`,
/// or hardware status code on failure. Callers that want to
/// distinguish "passthrough denied at runtime" from other IO errors
/// should match on the inner `io::ErrorKind`.
pub(crate) fn nvme_flush_ioctl(nvme_fd: RawFd, nsid: u32) -> Result<()> {
    // `nvme_passthru_cmd` layout per `linux/nvme_ioctl.h` (kernel
    // ≥ 4.12 stable). 64-byte struct, all fields little-endian on
    // x86_64 / aarch64.
    #[repr(C)]
    #[derive(Default)]
    struct NvmePassthruCmd {
        opcode: u8,
        flags: u8,
        rsvd1: u16,
        nsid: u32,
        cdw2: u32,
        cdw3: u32,
        metadata: u64,
        addr: u64,
        metadata_len: u32,
        data_len: u32,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
        cdw13: u32,
        cdw14: u32,
        cdw15: u32,
        timeout_ms: u32,
        result: u32,
    }

    // NVME_IOCTL_IO_CMD = _IOWR('N', 0x43, struct nvme_passthru_cmd)
    // For x86_64, _IOWR with size 64 bytes ('N' = 0x4e, type 0x43):
    //   dir=3 (RW) << 30 | size=64 << 16 | 'N' << 8 | nr=0x43
    //   = 0xc040_4e43.
    const NVME_IOCTL_IO_CMD: libc::c_ulong = 0xc040_4e43;

    let mut cmd = NvmePassthruCmd {
        opcode: 0x00, // FLUSH
        nsid,
        ..Default::default()
    };

    // SAFETY: `nvme_fd` is owned by the caller (an open `/dev/nvmeX`
    // file) for the duration of this synchronous call. `&mut cmd`
    // points to a stack-allocated `NvmePassthruCmd` of exactly the
    // size the kernel expects (matched by the ioctl request code's
    // size field). `ioctl` returns -1 on error rather than
    // panicking; we surface `errno` via `last_os_error`.
    let rc = unsafe { libc::ioctl(nvme_fd, NVME_IOCTL_IO_CMD, &mut cmd) };
    if rc < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Resolves `fd` to its NVMe character device path
/// (e.g. `/dev/nvme0`).
///
/// Walks `fstat(fd)` → `st_dev` → `/sys/dev/block/<major>:<minor>` →
/// readlink → trim namespace suffix. Returns `None` for non-block-
/// device fds, non-NVMe block devices, or any IO error along the
/// way.
fn nvme_char_device_for(fd: RawFd) -> Option<std::path::PathBuf> {
    // SAFETY: `libc::stat` is a plain-old-data C struct whose
    // bit pattern of all-zeros is a valid initialization (every
    // field is an integer or pointer that accepts zero); we
    // overwrite it via `fstat` before reading.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is a valid open file descriptor owned by the
    // caller for the duration of this call. `&mut stat` points to a
    // properly aligned `libc::stat` on this stack frame; fstat
    // writes through it before returning.
    let rc = unsafe { libc::fstat(fd, &mut stat) };
    if rc != 0 {
        return None;
    }
    let dev = stat.st_dev;
    let major = libc::major(dev);
    let minor = libc::minor(dev);
    let block_link = format!("/sys/dev/block/{major}:{minor}");
    let resolved = std::fs::canonicalize(&block_link).ok()?;
    // resolved looks like `/sys/devices/.../block/nvme0n1`. The
    // character device for that namespace is `/dev/nvme0`.
    let name = resolved.file_name()?.to_str()?;
    if !name.starts_with("nvme") {
        return None;
    }
    // `nvme0n1` -> `nvme0`. `nvme0n1p3` -> `nvme0`.
    let controller = name.split('n').next()?;
    if controller.is_empty() || !controller.starts_with("nvme") {
        return None;
    }
    Some(std::path::PathBuf::from(format!("/dev/{controller}")))
}

/// Reads the namespace ID for a block-device fd from
/// `/sys/block/<dev>/nsid`. Defaults to 1 when the file is missing
/// or unreadable (consumer NVMe drives universally use NSID 1 for
/// the primary namespace).
fn nvme_namespace_id_for(fd: RawFd) -> Option<u32> {
    // SAFETY: `libc::stat` is a plain-old-data C struct whose
    // bit pattern of all-zeros is a valid initialization (every
    // field is an integer or pointer that accepts zero); we
    // overwrite it via `fstat` before reading.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: same as `nvme_char_device_for` — fd is valid, stat is
    // on this stack frame.
    let rc = unsafe { libc::fstat(fd, &mut stat) };
    if rc != 0 {
        return None;
    }
    let dev = stat.st_dev;
    let major = libc::major(dev);
    let minor = libc::minor(dev);
    let nsid_path = format!("/sys/dev/block/{major}:{minor}/nsid");
    let s = std::fs::read_to_string(&nsid_path).ok()?;
    s.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicU32, Ordering};

    static C: AtomicU32 = AtomicU32::new(0);

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_iouring_test_{}_{}_{}",
            std::process::id(),
            n,
            tag
        ))
    }

    /// Try to construct a ring; `None` means the test environment
    /// lacks `io_uring_setup` access. The fallback path (`pwrite` +
    /// `fdatasync`) is exercised by the existing `Method::Direct`
    /// integration tests, so skipping these here doesn't reduce
    /// coverage on sandboxed runners.
    fn ring_or_skip() -> Option<IoUringRing> {
        match IoUringRing::new(8, None) {
            Ok(r) => Some(r),
            Err(Error::IoUringSetupFailed { .. }) => None,
            Err(e) => panic!("unexpected ring construction error: {e:?}"),
        }
    }

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn ring_construction_returns_ring_or_setup_failed() {
        match IoUringRing::new(8, None) {
            Ok(_) => {}
            Err(Error::IoUringSetupFailed { .. }) => {}
            Err(e) => panic!("unexpected variant: {e:?}"),
        }
    }

    #[test]
    fn write_at_round_trip() {
        let Some(ring) = ring_or_skip() else { return };
        let path = tmp_path("write_rt");
        let _g = Cleanup(path.clone());
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let data = vec![0xA5u8; 4096];
        let n = ring.write_at(f.as_raw_fd(), &data, 0).expect("write_at");
        assert_eq!(n, data.len());
        ring.fdatasync(f.as_raw_fd()).expect("fdatasync");
        let read_back = std::fs::read(&path).expect("read");
        assert_eq!(read_back, data);
    }

    #[test]
    fn read_at_round_trip() {
        let Some(ring) = ring_or_skip() else { return };
        let path = tmp_path("read_rt");
        let _g = Cleanup(path.clone());
        let data = vec![0x5Au8; 4096];
        std::fs::write(&path, &data).unwrap();
        let f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut buf = vec![0u8; 4096];
        let n = ring.read_at(f.as_raw_fd(), &mut buf, 0).expect("read_at");
        assert_eq!(n, data.len());
        assert_eq!(buf, data);
    }

    #[test]
    fn concurrent_submitters_serialise_through_owner() {
        let Some(ring) = ring_or_skip() else { return };
        let ring = std::sync::Arc::new(ring);
        let path = tmp_path("concurrent");
        let _g = Cleanup(path.clone());
        // Pre-size: 16 separate sectors so each submitter writes a
        // disjoint range and the assertion is order-independent.
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&vec![0u8; 16 * 4096]).unwrap();
        drop(f);

        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fd = f.as_raw_fd();

        let mut handles = Vec::new();
        for i in 0..16usize {
            let ring = ring.clone();
            let payload = vec![i as u8; 4096];
            handles.push(std::thread::spawn(move || {
                ring.write_at(fd, &payload, (i * 4096) as u64).unwrap()
            }));
        }
        for h in handles {
            assert_eq!(h.join().unwrap(), 4096);
        }
        ring.fdatasync(fd).unwrap();
        drop(f);

        let bytes = std::fs::read(&path).unwrap();
        for i in 0..16 {
            let slice = &bytes[i * 4096..(i + 1) * 4096];
            assert!(
                slice.iter().all(|&b| b == i as u8),
                "sector {i} content drift — owner-thread serialisation broken",
            );
        }
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.7 — IORING_REGISTER_FILES fd-slot coverage
    // ─────────────────────────────────────────────────────────
    //
    // The sync ring's `FdRegistry` maintains a 16-slot sparse
    // file table; per-op fds are lazily upgraded via
    // `register_files_update` on first use and cached for
    // subsequent submissions. These tests exercise both the
    // table-allocation path AND the table-full fallback to
    // raw-fd SQEs.
    //
    // Pre-0.9.7 these paths were never directly tested — the
    // 0.9.5 integration shipped without coverage and the 0.9.6
    // defensive disable removed them from runtime. The 0.9.7
    // restoration brings them back with these tests as the
    // regression guard.

    #[test]
    fn writes_across_many_distinct_fds_complete_correctly() {
        // Open 20 distinct files (4 over SLOT_TABLE_SIZE = 16)
        // and write a unique payload to each. With the slot
        // registry active, the first 16 fds get
        // `types::Fixed(slot)` SQEs and the remaining 4 fall
        // back to `types::Fd(raw)`. With the registry inactive
        // (the slot-table init disabled), every SQE uses
        // raw-fd. Either path must produce byte-for-byte
        // correct writes.
        let Some(ring) = ring_or_skip() else { return };
        const N_FDS: usize = 20;
        const PAYLOAD_LEN: usize = 256;

        let mut paths = Vec::with_capacity(N_FDS);
        let mut guards = Vec::with_capacity(N_FDS);
        let mut files = Vec::with_capacity(N_FDS);
        for i in 0..N_FDS {
            let path = tmp_path(&format!("manyfds_{i:02}"));
            guards.push(Cleanup(path.clone()));
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            files.push(f);
            paths.push(path);
        }

        for (i, f) in files.iter().enumerate() {
            let payload = vec![i as u8; PAYLOAD_LEN];
            let n = ring.write_at(f.as_raw_fd(), &payload, 0).expect("write_at");
            assert_eq!(n, PAYLOAD_LEN, "fd {i}: short write");
            ring.fdatasync(f.as_raw_fd()).expect("fdatasync");
        }
        drop(files);

        for (i, path) in paths.iter().enumerate() {
            let bytes = std::fs::read(path).expect("read");
            assert_eq!(
                bytes.len(),
                PAYLOAD_LEN,
                "fd {i}: wrong file size on read-back"
            );
            assert!(
                bytes.iter().all(|&b| b == i as u8),
                "fd {i}: content drift — slot/fd mapping bug"
            );
        }
    }

    #[test]
    fn repeated_writes_on_same_fd_round_trip() {
        // 32 writes on a single fd. With the slot registry
        // active, the first write should register the fd in
        // slot 0 and the remaining 31 should hit the cached
        // slot (no further `register_files_update` syscalls).
        // With the registry inactive, every write uses raw-fd.
        // Either path must place every payload at the right
        // offset with no content aliasing.
        let Some(ring) = ring_or_skip() else { return };
        const N_WRITES: usize = 32;
        const PAYLOAD_LEN: usize = 64;

        let path = tmp_path("slot_cache");
        let _g = Cleanup(path.clone());
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::write(&path, vec![0u8; N_WRITES * PAYLOAD_LEN]).unwrap();
        let fd = f.as_raw_fd();

        for i in 0..N_WRITES {
            let payload = vec![(i & 0xFF) as u8; PAYLOAD_LEN];
            let n = ring
                .write_at(fd, &payload, (i * PAYLOAD_LEN) as u64)
                .expect("write_at");
            assert_eq!(n, PAYLOAD_LEN, "iter {i}: short write");
        }
        ring.fdatasync(fd).expect("fdatasync");
        drop(f);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), N_WRITES * PAYLOAD_LEN);
        for i in 0..N_WRITES {
            let slice = &bytes[i * PAYLOAD_LEN..(i + 1) * PAYLOAD_LEN];
            let expected = (i & 0xFF) as u8;
            assert!(
                slice.iter().all(|&b| b == expected),
                "iter {i}: content drift (expected {expected}, got {:?}...)",
                &slice[..4]
            );
        }
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.4 — Linked write+fsync + NAWUN/NAWUPF parser
    // ─────────────────────────────────────────────────────────

    #[test]
    fn write_at_linked_fsync_round_trips_under_owner_thread() {
        // End-to-end: submit a linked Write + Fsync(DATASYNC)
        // chain via the new API. The owner thread pushes two
        // SQEs with IOSQE_IO_LINK and waits for both CQEs. We
        // verify the byte count comes back correct and the
        // content is on disk after sync_data has run.
        let Some(ring) = ring_or_skip() else { return };
        let path = tmp_path("linked_write_fsync");
        let _g = Cleanup(path.clone());
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let fd = f.as_raw_fd();
        let payload = b"linked write + fsync";
        let n = ring.write_at_linked_fsync(fd, payload, 0).unwrap();
        assert_eq!(n, payload.len());
        drop(f);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, payload);
    }

    #[test]
    fn parse_nawun_nawupf_extracts_le_u16_at_offset_74_76() {
        // Construct a synthetic 4096-byte Identify Namespace
        // response with known values at bytes 74-75 (NAWUN) and
        // 76-77 (NAWUPF), both little-endian.
        let mut id = [0u8; 4096];
        // NAWUN = 0x0007 → "atomic for 8 logical blocks"
        id[74] = 0x07;
        id[75] = 0x00;
        // NAWUPF = 0x000F → "atomic for 16 logical blocks"
        id[76] = 0x0F;
        id[77] = 0x00;
        let (nawun, nawupf) = parse_nawun_nawupf(&id);
        assert_eq!(nawun, Some(7));
        assert_eq!(nawupf, Some(15));
    }

    #[test]
    fn parse_nawun_nawupf_sentinel_0xffff_reads_as_none() {
        // The NVMe sentinel 0xFFFF means "unsupported" — must
        // surface as None so callers don't mistake "65 535 LBA
        // atomic guarantee" for "no guarantee".
        let mut id = [0u8; 4096];
        id[74] = 0xFF;
        id[75] = 0xFF;
        id[76] = 0xFF;
        id[77] = 0xFF;
        let (nawun, nawupf) = parse_nawun_nawupf(&id);
        assert_eq!(nawun, None);
        assert_eq!(nawupf, None);
    }

    #[test]
    fn parse_nawun_nawupf_zero_means_one_block_guarantee() {
        // A value of 0 in NAWUN/NAWUPF is 0-based: it means
        // "atomic for exactly one logical block" (the base
        // NVMe per-LBA guarantee). It is NOT the sentinel.
        let id = [0u8; 4096];
        let (nawun, nawupf) = parse_nawun_nawupf(&id);
        assert_eq!(nawun, Some(0));
        assert_eq!(nawupf, Some(0));
    }
}
