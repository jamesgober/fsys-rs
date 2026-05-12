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
}
