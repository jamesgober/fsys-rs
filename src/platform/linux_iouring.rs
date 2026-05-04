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
    pub(crate) fn new(queue_depth: u32) -> Result<Self> {
        // Probe synchronously. Drop the probe ring before spawning;
        // reconstruction in the owner thread is microsecond-scale,
        // and channel transport of `IoUring` is awkward (it's
        // `!Sync`, and the cleaner pattern is to keep all
        // `IoUring`-typed values out of struct fields).
        match io_uring::IoUring::new(queue_depth) {
            Ok(_probe) => {}
            Err(source) => return Err(Error::IoUringSetupFailed { source }),
        }

        let cap = (queue_depth as usize).max(1).saturating_mul(2);
        let (tx, rx) = bounded::<Op>(cap);

        let join = thread::Builder::new()
            .name("fsys-iouring".to_string())
            .spawn(move || {
                owner_loop(queue_depth, rx);
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
fn owner_loop(queue_depth: u32, rx: Receiver<Op>) {
    let mut ring = match io_uring::IoUring::new(queue_depth) {
        Ok(r) => r,
        // The probe in `IoUringRing::new` already succeeded; if
        // reconstruction fails here it's a transient kernel issue.
        // The thread exits, and all subsequent submitter sends will
        // see channel closed and surface the failure to the caller.
        Err(_) => return,
    };

    while let Ok(op) = rx.recv() {
        match op {
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
                    buf_len as u32,
                )
                .offset(offset)
                .build();
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
                let entry = io_uring::opcode::Read::new(
                    io_uring::types::Fd(fd),
                    buf_ptr as *mut u8,
                    buf_len as u32,
                )
                .offset(offset)
                .build();
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
                let entry = io_uring::opcode::Fsync::new(io_uring::types::Fd(fd))
                    .flags(io_uring::types::FsyncFlags::DATASYNC)
                    .build();
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
        }
    }
}

fn io_err(msg: &'static str) -> Error {
    Error::Io(std::io::Error::other(msg))
}

fn owner_dead() -> Error {
    Error::Io(std::io::Error::other("io_uring owner thread terminated"))
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
        match IoUringRing::new(8) {
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
        match IoUringRing::new(8) {
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
}
