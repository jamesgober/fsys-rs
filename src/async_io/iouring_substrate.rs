//! Native io_uring async substrate — per-op `write_at` / `read_at`
//! / `fdatasync` wrappers that submit through [`AsyncIoUring`] and
//! convert raw kernel result codes into `Result<usize>` /
//! `Result<()>`.
//!
//! See [`crate::async_io::completion_driver`] for the owner-task
//! design rationale and the load-bearing panic-resilience
//! invariant. This module is the thin conversion layer between
//! that low-level primitive and the rest of the async layer.

#![cfg(all(target_os = "linux", feature = "async"))]
#![allow(dead_code)] // ICE-class workaround — same as completion_driver.rs.

use crate::async_io::completion_driver::{AsyncIoUring, Op};
use crate::{Error, Result};
use std::os::fd::RawFd;
use tokio::sync::oneshot;

/// Submit a `Write` SQE for `buf` at `offset` on `fd` and `.await`
/// completion through the per-handle async ring.
///
/// # Safety contract
///
/// The caller MUST hold the `&[u8]` borrow alive across this
/// `.await`. Rust's borrow checker enforces this at the call site
/// — the `Future` returned by this function captures `'a` from
/// `buf: &'a [u8]`. The kernel reads the buffer at the recorded
/// pointer/length before signalling completion via the CQ; the
/// awaiting submitter holds the borrow until the oneshot resolves.
pub(crate) async fn write_at_native(
    ring: &AsyncIoUring,
    fd: RawFd,
    buf: &[u8],
    offset: u64,
) -> Result<usize> {
    let buf_ptr = buf.as_ptr() as usize;
    let buf_len = buf.len();
    let (tx, rx) = oneshot::channel::<i32>();
    let op = Op::Write {
        fd,
        buf_ptr,
        buf_len,
        offset,
        reply: tx,
    };
    let code = ring.submit(op, rx).await?;
    decode_io_result(code).map(|n| n as usize)
}

/// Submit a `Read` SQE filling `buf` from `offset` on `fd`.
pub(crate) async fn read_at_native(
    ring: &AsyncIoUring,
    fd: RawFd,
    buf: &mut [u8],
    offset: u64,
) -> Result<usize> {
    let buf_ptr = buf.as_mut_ptr() as usize;
    let buf_len = buf.len();
    let (tx, rx) = oneshot::channel::<i32>();
    let op = Op::Read {
        fd,
        buf_ptr,
        buf_len,
        offset,
        reply: tx,
    };
    let code = ring.submit(op, rx).await?;
    decode_io_result(code).map(|n| n as usize)
}

/// Submit an `Fsync(DATASYNC)` SQE on `fd`.
pub(crate) async fn fdatasync_native(ring: &AsyncIoUring, fd: RawFd) -> Result<()> {
    let (tx, rx) = oneshot::channel::<i32>();
    let op = Op::Fdatasync { fd, reply: tx };
    let code = ring.submit(op, rx).await?;
    let _result_byte_count = decode_io_result(code)?;
    Ok(())
}

/// Convert the kernel result code returned by io_uring into a
/// fsys `Result`. Codes ≥ 0 are byte counts (or 0 for void ops);
/// negative values are `-errno`.
fn decode_io_result(code: i32) -> Result<i32> {
    if code < 0 {
        Err(Error::Io(std::io::Error::from_raw_os_error(-code)))
    } else {
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicU32, Ordering};

    static C: AtomicU32 = AtomicU32::new(0);

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_substrate_test_{}_{}_{}",
            std::process::id(),
            n,
            tag
        ))
    }

    fn ring_or_skip() -> Option<AsyncIoUring> {
        AsyncIoUring::new(8).ok()
    }

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// 0.9.6 hardening: wraps an async test body with a hard
    /// 15-second timeout. If the body hangs (e.g. an io_uring
    /// CQE that never lands), the test panics with a clear
    /// message instead of dragging CI for the GitHub Actions
    /// default job timeout (~6 hours).
    async fn with_timeout<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        const TIMEOUT_SECS: u64 = 15;
        match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), fut).await {
            Ok(v) => v,
            Err(_) => panic!(
                "test exceeded {TIMEOUT_SECS}s timeout — likely a hang in the async substrate"
            ),
        }
    }

    #[tokio::test]
    async fn write_at_native_round_trips() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let path = tmp_path("write");
            let _g = Cleanup(path.clone());

            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();

            let data = vec![0xA5u8; 4096];
            let n = write_at_native(&ring, f.as_raw_fd(), &data, 0)
                .await
                .expect("write_at_native");
            assert_eq!(n, data.len());
            fdatasync_native(&ring, f.as_raw_fd())
                .await
                .expect("fdatasync_native");

            drop(f);
            let read_back = std::fs::read(&path).expect("read");
            assert_eq!(read_back, data);

            ring.shutdown().await;
        })
        .await;
    }

    #[tokio::test]
    async fn read_at_native_round_trips() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let path = tmp_path("read");
            let _g = Cleanup(path.clone());
            let data = vec![0x5Au8; 4096];
            std::fs::write(&path, &data).unwrap();

            let f = OpenOptions::new().read(true).open(&path).unwrap();
            let mut buf = vec![0u8; 4096];
            let n = read_at_native(&ring, f.as_raw_fd(), &mut buf, 0)
                .await
                .expect("read_at_native");
            assert_eq!(n, data.len());
            assert_eq!(buf, data);

            ring.shutdown().await;
        })
        .await;
    }

    #[tokio::test]
    async fn write_at_invalid_fd_returns_io_error() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };

            let data = vec![0u8; 64];
            // fd -1 is invalid; kernel returns -EBADF (errno 9).
            let result = write_at_native(&ring, -1, &data, 0).await;
            assert!(matches!(result, Err(Error::Io(_))));

            ring.shutdown().await;
        })
        .await;
    }

    #[tokio::test]
    async fn concurrent_writes_complete_independently() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let ring = std::sync::Arc::new(ring);

            let path = tmp_path("concurrent");
            let _g = Cleanup(path.clone());
            // Pre-size the file with 16 sectors of zeros.
            std::fs::write(&path, vec![0u8; 16 * 4096]).unwrap();
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
                handles.push(tokio::spawn(async move {
                    write_at_native(&ring, fd, &payload, (i * 4096) as u64)
                        .await
                        .expect("concurrent write")
                }));
            }
            for h in handles {
                assert_eq!(h.await.unwrap(), 4096);
            }
            fdatasync_native(&ring, fd).await.expect("fdatasync");
            drop(f);

            let bytes = std::fs::read(&path).unwrap();
            for i in 0..16 {
                let slice = &bytes[i * 4096..(i + 1) * 4096];
                assert!(
                    slice.iter().all(|&b| b == i as u8),
                    "sector {i} content drift — concurrent submission broke ordering"
                );
            }

            // Cleanup. The JoinHandles' inner Arc clones were freed
            // when their tasks completed; only the outer `ring`
            // binding holds a ref now. Use `Arc::into_inner` to
            // recover the inner value for shutdown.
            if let Some(r) = std::sync::Arc::into_inner(ring) {
                r.shutdown().await;
            }
        })
        .await;
    }

    /// 0.9.7 — `IORING_REGISTER_FILES` fd-slot coverage:
    /// many-distinct-fds scenario.
    ///
    /// Opens N > `SLOT_TABLE_SIZE` (16) distinct files and writes
    /// a unique payload to each through the async substrate. With
    /// the slot table active, the first 16 fds get fixed-file
    /// slots (`types::Fixed(slot)` SQEs); the 17th onwards fall
    /// back to raw-fd SQEs (`types::Fd(raw)`). With the slot
    /// table inactive (initial registration failed or disabled),
    /// every SQE uses the raw-fd path. Either way the write
    /// semantics must be **identical** — bytes hit the right
    /// file at the right offset.
    ///
    /// This test exists specifically to exercise the fd-slot
    /// upgrade + table-full fallback paths. Pre-0.9.7 these
    /// paths were never directly tested — the 0.9.5 integration
    /// went straight to production and the 0.9.6 follow-up
    /// disabled them defensively without test coverage. The
    /// test is the regression guard for any future re-enable
    /// (or kernel-version interaction surprise).
    ///
    /// 20 distinct fds (4 over the slot-table size) — verifies
    /// both the slot-allocation path AND the fallback path
    /// within a single test run.
    #[tokio::test]
    async fn writes_across_many_distinct_fds_complete_correctly() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            const N_FDS: usize = 20;
            const PAYLOAD_LEN: usize = 256;

            // Open 20 distinct files. Each gets a unique payload
            // (the file index, repeated PAYLOAD_LEN times) so we
            // can verify no cross-contamination at read-back.
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

            // Submit one write per fd. Order matters here: this
            // populates the slot table in registration order
            // 0..15, then fds 16..19 hit the table-full
            // fallback path.
            for (i, f) in files.iter().enumerate() {
                let payload = vec![i as u8; PAYLOAD_LEN];
                let n = write_at_native(&ring, f.as_raw_fd(), &payload, 0)
                    .await
                    .expect("write_at_native");
                assert_eq!(n, PAYLOAD_LEN, "fd {i}: short write");
                fdatasync_native(&ring, f.as_raw_fd())
                    .await
                    .expect("fdatasync_native");
            }

            // Drop the file handles before reading so the writes
            // are committed and the read path sees a clean
            // file-system view.
            drop(files);

            // Verify every file has its expected unique payload.
            // Any cross-contamination (e.g., wrong slot index
            // mapped to wrong fd, or a stale slot reused) would
            // show up here as the wrong byte pattern.
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

            ring.shutdown().await;
        })
        .await;
    }

    /// 0.9.7 — `IORING_REGISTER_FILES` slot-cache coverage:
    /// repeated submissions on the same fd must reuse the
    /// cached slot (no extra `register_files_update` syscall)
    /// and produce identical write semantics.
    ///
    /// We can't directly observe whether a slot was used vs
    /// raw-fd from outside the kernel, but we CAN verify that
    /// many submissions on a single fd complete correctly. A
    /// regression in the slot-cache lookup (e.g., the cache
    /// returning a stale slot index for a closed/recycled fd)
    /// would surface as content corruption.
    #[tokio::test]
    async fn repeated_writes_on_same_fd_round_trip() {
        with_timeout(async {
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
            let fd = f.as_raw_fd();

            // Pre-size the file to N_WRITES * PAYLOAD_LEN.
            std::fs::write(&path, vec![0u8; N_WRITES * PAYLOAD_LEN]).unwrap();

            // 32 writes on the same fd — first write registers the
            // fd in slot 0 (if active), subsequent writes should
            // hit the cache. Each write places a distinct payload
            // at a distinct offset.
            for i in 0..N_WRITES {
                let payload = vec![(i & 0xFF) as u8; PAYLOAD_LEN];
                let n = write_at_native(&ring, fd, &payload, (i * PAYLOAD_LEN) as u64)
                    .await
                    .expect("write_at_native");
                assert_eq!(n, PAYLOAD_LEN, "iter {i}: short write");
            }
            fdatasync_native(&ring, fd).await.expect("fdatasync_native");
            drop(f);

            // Verify every region has its expected payload — no
            // slot-cache aliasing.
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

            ring.shutdown().await;
        })
        .await;
    }
}
