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

    /// Opens 20 distinct files and writes a unique payload to each
    /// through the async substrate while all of them stay open.
    /// Every write must land byte-for-byte in its own file.
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

            // Submit one write per fd.
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
            // Any cross-contamination shows up here as the wrong
            // byte pattern.
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

            ring.shutdown().await;
        })
        .await;
    }

    /// Many submissions on one fd must each land at their own
    /// offset with no content aliasing.
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

            // 32 writes on the same fd, each placing a distinct
            // payload at a distinct offset.
            for i in 0..N_WRITES {
                let payload = vec![(i & 0xFF) as u8; PAYLOAD_LEN];
                let n = write_at_native(&ring, fd, &payload, (i * PAYLOAD_LEN) as u64)
                    .await
                    .expect("write_at_native");
                assert_eq!(n, PAYLOAD_LEN, "iter {i}: short write");
            }
            fdatasync_native(&ring, fd).await.expect("fdatasync_native");
            drop(f);

            // Verify every region has its expected payload.
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

    /// Write through fd N to file A, then make fd N refer to file B
    /// (`dup2` reuses the number the way close + open does under
    /// load) and write again through the same ring. The second write
    /// must land in B; 1.1.0's fixed-file cache sent it to A.
    #[tokio::test]
    async fn test_native_write_after_fd_number_reuse_targets_new_file() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let path_a = tmp_path("fdreuse_a");
            let path_b = tmp_path("fdreuse_b");
            let _ga = Cleanup(path_a.clone());
            let _gb = Cleanup(path_b.clone());
            let open_rw = |p: &std::path::Path| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(p)
                    .unwrap()
            };
            let file_a = open_rw(&path_a);
            let file_b = open_rw(&path_b);
            let fd = file_a.as_raw_fd();

            let payload_a = vec![b'A'; 5000];
            let n = write_at_native(&ring, fd, &payload_a, 0)
                .await
                .expect("write a");
            assert_eq!(n, payload_a.len());

            // SAFETY: both descriptors are open and owned by this
            // test. `dup2` atomically closes `fd` and makes the
            // number refer to file B; `file_a` still owns the number
            // and closes it on drop, so nothing is closed twice.
            let rc = unsafe { libc::dup2(file_b.as_raw_fd(), fd) };
            assert_eq!(rc, fd, "dup2 failed: {}", std::io::Error::last_os_error());

            let payload_b = vec![b'B'; 3000];
            let n = write_at_native(&ring, fd, &payload_b, 0)
                .await
                .expect("write b");
            assert_eq!(n, payload_b.len());
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
        })
        .await;
    }
}
