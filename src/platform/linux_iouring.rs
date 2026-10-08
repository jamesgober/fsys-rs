//! Linux `io_uring` submission wrapper (owner-thread design).
//!
//! ## Design: owner thread instead of `Mutex<IoUring>`
//!
//! `io_uring::IoUring` is `!Sync` (the SQ/CQ rings are SPSC). The
//! ring lives on a dedicated owner thread; callers forward
//! operations through a bounded `crossbeam_channel` and block on a
//! per-op reply channel.
//!
//! ## Throughput model
//!
//! Each [`IoUringRing`] serves one operation at a time: the owner
//! thread pushes the op's SQE (two for the linked write + fsync),
//! calls `submit_and_wait`, reaps the CQEs and replies before it
//! takes the next op off the channel. Concurrent callers on the same
//! Handle queue on the channel and are served in arrival order, so
//! the configured queue depth sizes the ring but does not keep
//! several ops in flight. What the ring buys over plain `pwrite` +
//! `fdatasync` is the linked write + fsync chain (one
//! `io_uring_enter(2)` instead of two syscalls), not parallelism.
//!
//! ## Buffer lifetime
//!
//! [`IoUringRing::write_at`] / [`IoUringRing::read_at`] forward the
//! buffer's raw pointer + length to the owner thread and **block**
//! until it replies. The owner thread replies only after the kernel
//! has posted a CQE for every SQE of the op: `submit_and_wait` is
//! retried across `EINTR` (a signal can cut the wait short after the
//! SQE was already consumed), so the caller's `&[u8]` / `&mut [u8]`
//! borrow always outlives the kernel's use of the memory. If the
//! kernel refuses a submission outright, the SQEs never left the
//! submission queue; the owner replies with an error and shuts the
//! ring down so the stale SQE can never be submitted later.
//!
//! ## Large transfers
//!
//! One SQE carries at most [`MAX_SQE_LEN`] bytes (the kernel caps a
//! single read or write at `MAX_RW_COUNT`, just under 2 GiB, in any
//! case). [`IoUringRing::write_at`], [`IoUringRing::read_at`] and
//! [`IoUringRing::write_at_fixed`] split larger buffers and loop on
//! short transfers, so no length is ever truncated into the SQE's
//! 32-bit length field.
//!
//! ## Failure semantics
//!
//! Construction failure (kernel < 5.1, SECCOMP/AppArmor block, no
//! permission, or owner-thread spawn failure) returns
//! [`Error::IoUringSetupFailed`]. Per locked decision #1 + R-2''' in
//! `.dev/DECISIONS-0.5.0.md`, callers (the `Method::Direct` backend
//! in `crud/file.rs`) catch the error and fall back to `O_DIRECT` +
//! `pwrite` + `fdatasync`. `active_method` is **not** downgraded;
//! the durability contract is identical.

#![cfg(target_os = "linux")]

use crate::{Error, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::os::fd::RawFd;
use std::thread::{self, JoinHandle};

/// Largest byte count placed in a single read/write SQE.
///
/// The kernel caps one read or write at `MAX_RW_COUNT`
/// (`INT_MAX & PAGE_MASK`), so a longer request would come back
/// short anyway. `0x7fff_0000` is below that cap for every page size
/// up to 64 KiB and is a multiple of 64 KiB, so a chunk boundary
/// never breaks `O_DIRECT` sector alignment. It also fits the SQE's
/// `u32` length field.
pub(crate) const MAX_SQE_LEN: usize = 0x7fff_0000;

/// Per-handle io_uring submission ring.
///
/// Constructed lazily by [`crate::handle::Handle::io_uring_ring`] on
/// the first Direct-method op when the configured method matches.
/// Idle handles cost zero ring memory and no spawned threads. See
/// the module docs for the one-op-at-a-time throughput model.
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
///
/// Every `buf_len` is at most [`MAX_SQE_LEN`]; the public methods on
/// [`IoUringRing`] split longer buffers before sending.
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
    ///
    /// The reply carries `(bytes_written, fsync_ran)`. A short write
    /// breaks the link, the kernel cancels the fsync, and
    /// `fsync_ran` comes back `false`.
    WriteLinkedFsync {
        fd: RawFd,
        buf_ptr: usize,
        buf_len: usize,
        offset: u64,
        reply: Sender<Result<(usize, bool)>>,
    },
    /// 0.9.6: register a fixed set of buffers with the ring via
    /// `IORING_REGISTER_BUFFERS`. The kernel pins the buffer
    /// pages, hands back slot indices, and subsequent
    /// `Op::WriteFixed` submissions reference the buffer by
    /// slot index rather than re-mapping pages every SQE.
    ///
    /// The `iovs` carry (ptr_as_usize, len) tuples. The reply
    /// is `Result<()>`: the kernel reports success/failure for
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
    /// page-table lookups, which pays off on the journal hot path
    /// that reuses the LogBuffer's two AlignedBuf slots thousands
    /// of times.
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
        // and the cleaner pattern is to keep the `!Sync` `IoUring`
        // value on the owner thread only.
        //
        // 0.9.4: the probe builds with the setup flags
        // (`COOP_TASKRUN` / `SINGLE_ISSUER` / `DEFER_TASKRUN`) that
        // the host kernel supports. The probe in
        // `iouring_features::features()` happens at most once per
        // process; ring construction here just calls
        // `apply(&mut builder)` to set the cached bits.
        //
        // 0.9.7 SQPOLL: when the caller opts in via
        // `Builder::sqpoll(idle_ms)`, enable `IORING_SETUP_SQPOLL`,
        // which spawns a kernel-side polling thread to drain the
        // submission queue without requiring `io_uring_enter`
        // syscalls. May fail with `EPERM` on kernels < 5.13 without
        // `CAP_SYS_NICE`, in sandboxed containers, or under
        // restrictive SECCOMP. On setup failure we bubble the error
        // up as `IoUringSetupFailed`; the caller's `iouring_slot`
        // then flips to `Disabled` and the Direct path falls back to
        // `pwrite`, same contract as any other io_uring setup
        // failure. SQPOLL rings before Linux 5.11 only accept
        // registered files; since every SQE here carries a raw fd,
        // ops on such kernels fail and the Direct path falls back to
        // `pwrite` per op.
        let mut probe_builder = io_uring::IoUring::builder();
        super::iouring_features::apply(&mut probe_builder, super::iouring_features::RingMode::Sync);
        if let Some(idle_ms) = sqpoll_idle_ms {
            let _ = probe_builder.setup_sqpoll(idle_ms);
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

    /// Writes all of `buf` at `offset` on `fd`, one SQE per
    /// [`MAX_SQE_LEN`] chunk, and returns the number of bytes
    /// written.
    ///
    /// Short writes are retried for the remainder; the result is
    /// below `buf.len()` only when the kernel reports zero progress.
    /// The caller's `&[u8]` borrow is held alive across every
    /// blocking reply receive.
    pub(crate) fn write_at(&self, fd: RawFd, buf: &[u8], offset: u64) -> Result<usize> {
        let base = buf.as_ptr() as usize;
        transfer(buf.len(), offset, |start, len, off| {
            let (rt, rr) = bounded::<Result<usize>>(1);
            self.send(Op::Write {
                fd,
                buf_ptr: base + start,
                buf_len: len,
                offset: off,
                reply: rt,
            })?;
            rr.recv().map_err(|_| owner_dead())?
        })
    }

    /// Fills `buf` from `offset` on `fd`, one SQE per
    /// [`MAX_SQE_LEN`] chunk, and returns the number of bytes read.
    ///
    /// The result is below `buf.len()` only at end of file.
    pub(crate) fn read_at(&self, fd: RawFd, buf: &mut [u8], offset: u64) -> Result<usize> {
        let base = buf.as_mut_ptr() as usize;
        transfer(buf.len(), offset, |start, len, off| {
            let (rt, rr) = bounded::<Result<usize>>(1);
            self.send(Op::Read {
                fd,
                buf_ptr: base + start,
                buf_len: len,
                offset: off,
                reply: rt,
            })?;
            rr.recv().map_err(|_| owner_dead())?
        })
    }

    /// Submits an `Fsync(DATASYNC)` SQE on `fd`. Equivalent to
    /// `fdatasync(2)` for durability.
    pub(crate) fn fdatasync(&self, fd: RawFd) -> Result<()> {
        let (rt, rr) = bounded::<Result<()>>(1);
        self.send(Op::Fdatasync { fd, reply: rt })?;
        rr.recv().map_err(|_| owner_dead())?
    }

    /// 0.9.4: Writes `buf` at `offset` on `fd` and makes it durable
    /// with `fdatasync` semantics, returning the number of bytes
    /// written.
    ///
    /// When `buf` fits one SQE the write and an `Fsync(DATASYNC)`
    /// are submitted as one linked chain (`IOSQE_IO_LINK`), so both
    /// run for the price of a single `io_uring_enter(2)`. A short
    /// write breaks the chain and the kernel cancels the fsync; the
    /// remainder is then written with [`Self::write_at`] and synced
    /// with [`Self::fdatasync`]. Buffers longer than
    /// [`MAX_SQE_LEN`] take that unlinked path directly.
    ///
    /// On error the caller MUST assume the data is not durable.
    pub(crate) fn write_at_linked_fsync(
        &self,
        fd: RawFd,
        buf: &[u8],
        offset: u64,
    ) -> Result<usize> {
        if buf.len() > MAX_SQE_LEN {
            let written = self.write_at(fd, buf, offset)?;
            self.fdatasync(fd)?;
            return Ok(written);
        }
        let (rt, rr) = bounded::<Result<(usize, bool)>>(1);
        self.send(Op::WriteLinkedFsync {
            fd,
            buf_ptr: buf.as_ptr() as usize,
            buf_len: buf.len(),
            offset,
            reply: rt,
        })?;
        let (written, fsync_ran) = rr.recv().map_err(|_| owner_dead())??;
        if written >= buf.len() {
            if !fsync_ran {
                self.fdatasync(fd)?;
            }
            return Ok(written);
        }
        let rest = self.write_at(fd, &buf[written..], advance(offset, written)?)?;
        self.fdatasync(fd)?;
        Ok(written + rest)
    }

    /// 0.9.6: Register a fixed set of buffers with the ring.
    ///
    /// Each `(ptr, len)` tuple in `iovs` becomes a registered
    /// buffer slot at index `0..iovs.len()`. The caller is
    /// responsible for keeping the underlying memory alive
    /// (un-moved, not freed) for the lifetime of the ring;
    /// io_uring pins the pages but doesn't take ownership.
    ///
    /// Slot indices `0..iovs.len()` are then usable as the
    /// `buf_idx` argument to [`Self::write_at_fixed`].
    ///
    /// Returns `Err` on registration failure (kernel rejection,
    /// privilege denial, out-of-resource). On error, no slots
    /// are partially registered; the call is atomic.
    pub(crate) fn register_buffers(&self, iovs: &[(usize, usize)]) -> Result<()> {
        let (rt, rr) = bounded::<Result<()>>(1);
        self.send(Op::RegisterBuffers {
            iovs: iovs.to_vec(),
            reply: rt,
        })?;
        rr.recv().map_err(|_| owner_dead())?
    }

    /// 0.9.6: Writes all of `buf` with `IORING_OP_WRITE_FIXED`.
    ///
    /// `buf_idx` references a slot previously registered via
    /// [`Self::register_buffers`] and `buf` must lie within that
    /// registered buffer; the kernel validates the range. Saves the
    /// per-SQE page-pinning cost of `Op::Write` because the pages
    /// were pinned once at registration time. Chunks and short
    /// writes are handled like [`Self::write_at`].
    pub(crate) fn write_at_fixed(
        &self,
        fd: RawFd,
        buf_idx: u16,
        buf: &[u8],
        offset: u64,
    ) -> Result<usize> {
        let base = buf.as_ptr() as usize;
        transfer(buf.len(), offset, |start, len, off| {
            let (rt, rr) = bounded::<Result<usize>>(1);
            self.send(Op::WriteFixed {
                fd,
                buf_idx,
                buf_ptr: base + start,
                buf_len: len,
                offset: off,
                reply: rt,
            })?;
            rr.recv().map_err(|_| owner_dead())?
        })
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
            // The owner thread never panics on its own (every
            // fallible step replies with an error); if it did, there
            // is nothing left to clean up and Drop cannot report it.
            let _ = j.join();
        }
    }
}

/// Splits a `total`-byte transfer starting at file `offset` into
/// chunks of at most [`MAX_SQE_LEN`] bytes and runs `step(start, len,
/// file_offset)` for each, continuing after short transfers.
///
/// Returns the byte count moved; it is below `total` only when a step
/// reports zero progress (end of file for reads).
fn transfer(
    total: usize,
    offset: u64,
    mut step: impl FnMut(usize, usize, u64) -> Result<usize>,
) -> Result<usize> {
    let mut done = 0usize;
    while done < total {
        let len = (total - done).min(MAX_SQE_LEN);
        let n = step(done, len, advance(offset, done)?)?;
        if n == 0 {
            break;
        }
        // A step never reports more than it was asked for; `min`
        // keeps `done <= total` even if the kernel misbehaves.
        done += n.min(len);
    }
    Ok(done)
}

/// `offset + done` as a file offset, rejecting overflow.
fn advance(offset: u64, done: usize) -> Result<u64> {
    u64::try_from(done)
        .ok()
        .and_then(|d| offset.checked_add(d))
        .ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io_uring transfer offset overflows u64",
            ))
        })
}

/// SQE length field for a chunk the caller already bounded by
/// [`MAX_SQE_LEN`].
fn sqe_len(len: usize) -> u32 {
    // MAX_SQE_LEN < u32::MAX, so after `min` the conversion cannot
    // fail; the fallback only keeps this function total.
    u32::try_from(len.min(MAX_SQE_LEN)).unwrap_or(0)
}

/// Converts a CQE result into a byte count or the `-errno` it
/// carries.
fn cqe_bytes(res: i32) -> Result<usize> {
    usize::try_from(res).map_err(|_| cqe_error(res))
}

/// Builds the error for a negative CQE result (`-errno`).
fn cqe_error(res: i32) -> Error {
    Error::Io(std::io::Error::from_raw_os_error(res.saturating_neg()))
}

/// Outcome of [`drive`].
enum Drive<const N: usize> {
    /// The kernel posted a CQE for every SQE. Results are indexed by
    /// SQE position.
    Done([i32; N]),
    /// The kernel refused the submission before consuming any SQE.
    /// The entries may still sit in the submission queue, so the ring
    /// must not be used again.
    Rejected(Error),
}

/// Pushes `entries` as one batch, submits them and blocks until the
/// kernel has posted a CQE for each.
///
/// `submit_and_wait` can return before the CQEs exist: a signal
/// delivered to the owner thread interrupts the wait after the SQEs
/// were consumed, and `EAGAIN` / `EBUSY` report transient resource
/// pressure. All of these are retried, because returning while an
/// SQE is in flight would let the caller free memory the kernel is
/// still reading or writing. Only when the kernel refuses the
/// submission with every SQE still unconsumed (and no SQPOLL thread
/// could pick it up later) does this return [`Drive::Rejected`].
///
/// # Safety
///
/// Every buffer an entry points at must stay valid, and must not be
/// accessed in a conflicting way, until this function returns.
unsafe fn drive<const N: usize>(
    ring: &mut io_uring::IoUring,
    entries: [io_uring::squeue::Entry; N],
) -> Drive<N> {
    let mut tagged = entries;
    for (idx, entry) in tagged.iter_mut().enumerate() {
        *entry = entry.clone().user_data(idx as u64);
    }
    // SAFETY: forwarded from this function's contract; the buffers
    // stay valid until we return, and we only return once the kernel
    // has finished with every entry or never consumed any of them.
    // `push_multiple` pushes all entries or none.
    if unsafe { ring.submission().push_multiple(&tagged) }.is_err() {
        return Drive::Rejected(Error::Io(std::io::Error::other(
            "io_uring submission queue full",
        )));
    }
    let sqpoll = ring.params().is_setup_sqpoll();
    let mut results = [0i32; N];
    let mut seen = [false; N];
    let mut reaped = 0usize;
    while reaped < N {
        if let Err(e) = ring.submit_and_wait(N - reaped) {
            let transient = matches!(
                e.raw_os_error(),
                Some(libc::EINTR | libc::EAGAIN | libc::EBUSY)
            );
            let none_consumed = ring.submission().len() == N;
            if !transient && !sqpoll && reaped == 0 && none_consumed {
                return Drive::Rejected(Error::Io(e));
            }
            if !transient {
                // At least one SQE is in flight, so its buffer is
                // still in use. Keep waiting instead of replying;
                // back off so a persistent error does not spin.
                thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        loop {
            let next = ring.completion().next();
            let Some(cqe) = next else { break };
            let idx = usize::try_from(cqe.user_data()).unwrap_or(usize::MAX);
            if idx < N && !seen[idx] {
                seen[idx] = true;
                results[idx] = cqe.result();
                reaped += 1;
            }
        }
    }
    Drive::Done(results)
}

/// Delivers an op's result to the blocked caller and reports whether
/// the ring can keep serving ops.
fn reply_with<T, const N: usize>(
    reply: Sender<Result<T>>,
    outcome: Drive<N>,
    finish: impl FnOnce([i32; N]) -> Result<T>,
) -> bool {
    let (result, healthy) = match outcome {
        Drive::Done(results) => (finish(results), true),
        Drive::Rejected(e) => (Err(e), false),
    };
    // Callers block on the reply until it arrives, so the receiver is
    // gone only if the calling thread died; there is no one left to
    // deliver the result to.
    let _ = reply.send(result);
    healthy
}

/// Owner-thread main loop.
///
/// All `io_uring::IoUring` interaction lives here and in [`drive`].
/// Ops are served strictly one at a time (see the module docs).
fn owner_loop(queue_depth: u32, rx: Receiver<Op>, sqpoll_idle_ms: Option<u32>) {
    // 0.9.4: build with the same setup flags the `IoUringRing::new`
    // probe accepted. `iouring_features::apply` reads the
    // process-cached probe result, so no second kernel probe happens
    // here. 0.9.7 SQPOLL: re-apply the same SQPOLL toggle; the probe
    // ring was dropped before this thread spawned.
    let mut builder = io_uring::IoUring::builder();
    super::iouring_features::apply(&mut builder, super::iouring_features::RingMode::Sync);
    if let Some(idle_ms) = sqpoll_idle_ms {
        let _ = builder.setup_sqpoll(idle_ms);
    }
    let mut ring = match builder.build(queue_depth) {
        Ok(r) => r,
        // The probe in `IoUringRing::new` already succeeded; if
        // reconstruction fails here it's a transient kernel issue.
        // The thread exits, and all subsequent submitter sends will
        // see channel closed and surface the failure to the caller.
        Err(_) => return,
    };

    // Every SQE carries the caller's raw fd (`types::Fd`). 1.1.1
    // removed the `IORING_REGISTER_FILES` slot cache that used to
    // live here: it was keyed by fd number and never invalidated,
    // so once a file closed and the kernel reused its fd number for
    // a different file, the cached `types::Fixed(slot)` still
    // pointed at the old file (the registered table holds its own
    // reference) and writes landed in the wrong file. The ring is
    // shared by every op on the Handle, so short-lived temp files
    // hit this on the second write.
    while let Ok(op) = rx.recv() {
        let healthy = match op {
            Op::Write {
                fd,
                buf_ptr,
                buf_len,
                offset,
                reply,
            } => {
                let entry = io_uring::opcode::Write::new(
                    io_uring::types::Fd(fd),
                    buf_ptr as *const u8,
                    sqe_len(buf_len),
                )
                .offset(offset)
                .build();
                // SAFETY: the submitter (`IoUringRing::write_at`) is
                // blocked on `reply` until we send, which keeps its
                // `&[u8]` borrow (at least `buf_len` readable bytes
                // at `buf_ptr`) alive. `drive` returns only after the
                // kernel has finished with the SQE.
                let outcome = unsafe { drive(&mut ring, [entry]) };
                reply_with(reply, outcome, |[res]| cqe_bytes(res))
            }

            Op::Read {
                fd,
                buf_ptr,
                buf_len,
                offset,
                reply,
            } => {
                let entry = io_uring::opcode::Read::new(
                    io_uring::types::Fd(fd),
                    buf_ptr as *mut u8,
                    sqe_len(buf_len),
                )
                .offset(offset)
                .build();
                // SAFETY: the submitter (`IoUringRing::read_at`) is
                // blocked on `reply`, keeping its `&mut [u8]` borrow
                // (at least `buf_len` writable bytes at `buf_ptr`,
                // not accessed by anyone else) alive until `drive`
                // has seen the CQE.
                let outcome = unsafe { drive(&mut ring, [entry]) };
                reply_with(reply, outcome, |[res]| cqe_bytes(res))
            }

            Op::Fdatasync { fd, reply } => {
                let entry = io_uring::opcode::Fsync::new(io_uring::types::Fd(fd))
                    .flags(io_uring::types::FsyncFlags::DATASYNC)
                    .build();
                // SAFETY: the SQE references no memory. The fd stays
                // open because the submitter holds its file across
                // the blocking reply receive.
                let outcome = unsafe { drive(&mut ring, [entry]) };
                reply_with(reply, outcome, |[res]| cqe_bytes(res).map(|_| ()))
            }

            Op::WriteLinkedFsync {
                fd,
                buf_ptr,
                buf_len,
                offset,
                reply,
            } => {
                // 0.9.4: linked Write + Fsync(DATASYNC). The Write
                // SQE carries IOSQE_IO_LINK so the kernel runs the
                // Fsync only after the Write completes in full.
                let write_entry = io_uring::opcode::Write::new(
                    io_uring::types::Fd(fd),
                    buf_ptr as *const u8,
                    sqe_len(buf_len),
                )
                .offset(offset)
                .build()
                .flags(io_uring::squeue::Flags::IO_LINK);
                let fsync_entry = io_uring::opcode::Fsync::new(io_uring::types::Fd(fd))
                    .flags(io_uring::types::FsyncFlags::DATASYNC)
                    .build();
                // SAFETY: the submitter blocks on `reply`, keeping
                // its `&[u8]` borrow (`buf_len` bytes at `buf_ptr`)
                // alive; `drive` returns only after both CQEs.
                let outcome = unsafe { drive(&mut ring, [write_entry, fsync_entry]) };
                reply_with(reply, outcome, |[w, f]| {
                    // A failed write cancels the fsync; report the
                    // write's error.
                    let written = cqe_bytes(w)?;
                    if f >= 0 {
                        Ok((written, true))
                    } else if f == -libc::ECANCELED && written < buf_len {
                        // Short write broke the link. The caller
                        // writes the rest and syncs separately.
                        Ok((written, false))
                    } else {
                        // Write landed but fsync failed: the caller
                        // MUST treat the write as not durable.
                        Err(cqe_error(f))
                    }
                })
            }

            Op::RegisterBuffers { iovs, reply } => {
                // 0.9.6: IORING_REGISTER_BUFFERS. Pin the caller's
                // buffer ranges in the kernel's page-table so
                // subsequent `WriteFixed` SQEs skip the
                // per-submission page-pinning hop.
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
                // `iovec_array` lives across the syscall; the
                // kernel only needs the iovec descriptors during
                // the call, not after.
                let result =
                    unsafe { ring.submitter().register_buffers(&iovec_array) }.map_err(Error::Io);
                // Same reasoning as in `reply_with`: the submitter
                // blocks on the reply, so a send failure means it
                // is gone.
                let _ = reply.send(result);
                true
            }

            Op::WriteFixed {
                fd,
                buf_idx,
                buf_ptr,
                buf_len,
                offset,
                reply,
            } => {
                // 0.9.6: IORING_OP_WRITE_FIXED. Uses a previously
                // registered buffer slot; the kernel skips per-SQE
                // page pinning.
                let entry = io_uring::opcode::WriteFixed::new(
                    io_uring::types::Fd(fd),
                    buf_ptr as *const u8,
                    sqe_len(buf_len),
                    buf_idx,
                )
                .offset(offset)
                .build();
                // SAFETY: the registered buffer is owned and kept
                // alive by the caller (LogBuffer holds the
                // AlignedBuf for its entire lifetime, longer than
                // this ring), and the submitter blocks on `reply`
                // until `drive` has seen the CQE. `buf_ptr` +
                // `buf_len` describe a sub-region of the registered
                // slot at `buf_idx`; the kernel validates the range.
                let outcome = unsafe { drive(&mut ring, [entry]) };
                reply_with(reply, outcome, |[res]| cqe_bytes(res))
            }
        };
        if !healthy {
            // A rejected submission may have left SQEs in the
            // submission queue that point at memory the caller has
            // now reclaimed. Stop serving: dropping `rx` makes every
            // later call fail fast (callers fall back to `pwrite`),
            // and dropping the ring discards the stale entries
            // without submitting them.
            return;
        }
    }
}

fn owner_dead() -> Error {
    Error::Io(std::io::Error::other("io_uring owner thread terminated"))
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
    // fd routing coverage
    // ─────────────────────────────────────────────────────────
    //
    // Every SQE carries the caller's raw fd. These tests pin that
    // writes land in the file the caller passed, across many
    // distinct fds and across fd-number reuse after close (the
    // 1.1.0 fixed-file cache got the reuse case wrong).

    /// Opens a raw read-write file for the fd-reuse tests.
    fn open_rw(path: &std::path::Path) -> std::fs::File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn test_ring_write_after_fd_number_reuse_targets_new_file() {
        // Write through fd N to file A, then make fd N refer to file
        // B (dup2 closes A's descriptor and reuses the number, which
        // is exactly what close + open does under load) and write
        // again through the same ring. The second write must land
        // in B; A must keep its own bytes.
        let Some(ring) = ring_or_skip() else { return };
        let path_a = tmp_path("fdreuse_a");
        let path_b = tmp_path("fdreuse_b");
        let _ga = Cleanup(path_a.clone());
        let _gb = Cleanup(path_b.clone());
        let file_a = open_rw(&path_a);
        let file_b = open_rw(&path_b);
        let fd = file_a.as_raw_fd();

        let payload_a = vec![b'A'; 5000];
        assert_eq!(ring.write_at(fd, &payload_a, 0).expect("write a"), 5000);

        // SAFETY: both descriptors are open and owned by this test.
        // `dup2` atomically closes `fd` and makes the number refer to
        // file B's open description; `file_a` still owns the number
        // and closes it (now pointing at B) on drop, so nothing is
        // closed twice.
        let rc = unsafe { libc::dup2(file_b.as_raw_fd(), fd) };
        assert_eq!(rc, fd, "dup2 failed: {}", std::io::Error::last_os_error());

        let payload_b = vec![b'B'; 3000];
        assert_eq!(ring.write_at(fd, &payload_b, 0).expect("write b"), 3000);
        ring.fdatasync(fd).expect("fdatasync");
        drop(file_a);
        drop(file_b);

        assert_eq!(
            std::fs::read(&path_a).unwrap(),
            payload_a,
            "file A was overwritten"
        );
        assert_eq!(
            std::fs::read(&path_b).unwrap(),
            payload_b,
            "file B missed its write"
        );
    }

    #[test]
    fn writes_across_many_distinct_fds_complete_correctly() {
        // Open 20 distinct files and write a unique payload to
        // each while all of them stay open. Every write must
        // land byte-for-byte in its own file.
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
                "fd {i}: content drift, bytes routed to the wrong file"
            );
        }
    }

    #[test]
    fn repeated_writes_on_same_fd_round_trip() {
        // 32 writes on a single fd. Every payload must land at
        // its own offset with no content aliasing.
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

    // ─────────────────────────────────────────────────────────
    // 1.1.1: completion waits and chunking
    // ─────────────────────────────────────────────────────────

    extern "C" fn ignore_signal(_: libc::c_int) {}

    /// Sends SIGUSR1 to every `fsys-iouring` owner thread in this
    /// process. A no-op handler is installed first so the signal only
    /// interrupts blocking syscalls.
    fn interrupt_owner_threads() -> usize {
        // SAFETY: `sigaction` is given a zeroed struct with a valid
        // handler and an empty mask; installing a do-nothing handler
        // for SIGUSR1 has no effect beyond interrupting syscalls.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = ignore_signal as extern "C" fn(libc::c_int) as usize;
            assert_eq!(libc::sigemptyset(&mut action.sa_mask), 0);
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()),
                0
            );
        }
        let mut hit = 0;
        for entry in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
            let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            if comm.trim() != "fsys-iouring" {
                continue;
            }
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|t| t.parse::<i32>().ok())
            else {
                continue;
            };
            // SAFETY: plain syscall on our own process; a stale tid
            // only yields ESRCH.
            let rc = unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGUSR1) };
            if rc == 0 {
                hit += 1;
            }
        }
        hit
    }

    #[test]
    fn test_ring_read_interrupted_by_signal_waits_for_completion() {
        // A read from an empty pipe stays in flight. Interrupt the
        // owner thread while it waits, then feed the pipe. The read
        // must complete with the fed bytes; 1.1.0 replied "completion
        // queue empty" on the interrupted wait and left the read in
        // flight against the caller's (by then freed) buffer.
        let Some(ring) = ring_or_skip() else { return };
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid two-element array for pipe(2).
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);

        let feeder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let signalled = interrupt_owner_threads();
            std::thread::sleep(std::time::Duration::from_millis(100));
            let msg = b"ping";
            // SAFETY: `write_fd` is the open write end of the pipe;
            // `msg` is valid for 4 bytes.
            let n = unsafe { libc::write(write_fd, msg.as_ptr().cast(), msg.len()) };
            assert_eq!(n, 4);
            signalled
        });

        let mut buf = [0u8; 4];
        let n = ring
            .read_at(read_fd, &mut buf, 0)
            .expect("interrupted read must complete");
        let signalled = feeder.join().unwrap();
        // SAFETY: both pipe ends are open and owned by this test.
        unsafe {
            assert_eq!(libc::close(read_fd), 0);
            assert_eq!(libc::close(write_fd), 0);
        }
        assert!(signalled >= 1, "no fsys-iouring thread found to interrupt");
        assert_eq!(n, 4);
        assert_eq!(&buf, b"ping");
    }

    #[test]
    fn test_transfer_splits_at_max_sqe_len() {
        let total = 2 * MAX_SQE_LEN + 5;
        let mut calls = Vec::new();
        let done = transfer(total, 100, |start, len, off| {
            calls.push((start, len, off));
            Ok(len)
        })
        .unwrap();
        assert_eq!(done, total);
        assert_eq!(
            calls,
            vec![
                (0, MAX_SQE_LEN, 100),
                (MAX_SQE_LEN, MAX_SQE_LEN, 100 + MAX_SQE_LEN as u64),
                (2 * MAX_SQE_LEN, 5, 100 + 2 * MAX_SQE_LEN as u64),
            ]
        );
        assert!(u32::try_from(MAX_SQE_LEN).is_ok());
        assert_eq!(sqe_len(usize::MAX), MAX_SQE_LEN as u32);
    }

    #[test]
    fn test_transfer_short_steps_resume_at_next_byte() {
        let mut calls = Vec::new();
        let done = transfer(10, 0, |start, len, off| {
            calls.push((start, len, off));
            Ok(len.min(4))
        })
        .unwrap();
        assert_eq!(done, 10);
        assert_eq!(calls, vec![(0, 10, 0), (4, 6, 4), (8, 2, 8)]);
    }

    #[test]
    fn test_transfer_zero_progress_returns_short_count() {
        let mut steps = 0;
        let done = transfer(10, 0, |_, len, _| {
            steps += 1;
            Ok(if steps == 1 { len.min(3) } else { 0 })
        })
        .unwrap();
        assert_eq!(done, 3);
        assert_eq!(steps, 2);
    }

    #[test]
    fn test_transfer_empty_input_makes_no_calls() {
        let done = transfer(0, 0, |_, _, _| panic!("no step expected")).unwrap();
        assert_eq!(done, 0);
    }

    #[test]
    fn test_transfer_step_error_propagates() {
        let err = transfer(10, 0, |_, _, _| Err(io_error_for_test())).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn test_advance_offset_overflow_returns_err() {
        assert_eq!(advance(5, 7).unwrap(), 12);
        assert!(advance(u64::MAX, 1).is_err());
    }

    #[test]
    fn test_cqe_bytes_negative_maps_to_errno() {
        assert_eq!(cqe_bytes(17).unwrap(), 17);
        match cqe_bytes(-libc::EBADF) {
            Err(Error::Io(e)) => assert_eq!(e.raw_os_error(), Some(libc::EBADF)),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn io_error_for_test() -> Error {
        Error::Io(std::io::Error::other("step failed"))
    }
}
