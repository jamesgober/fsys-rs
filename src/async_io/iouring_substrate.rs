//! Native io_uring async substrate: per-op `write_at` / `fdatasync`
//! wrappers that submit through [`AsyncIoUring`].
//!
//! See [`crate::async_io::completion_driver`] for the owner-task
//! design and the buffer-ownership rules. This module is the thin
//! layer the journal and CRUD async paths call.

#![cfg(all(target_os = "linux", feature = "async"))]

use crate::async_io::completion_driver::{AsyncIoUring, FileRef, IoBuf, Op};
use crate::Result;

/// Writes all of `buf` at `offset` on `file` through the async ring
/// and returns the number of bytes written.
///
/// `buf` and `file` move into the driver, which keeps them until the
/// kernel has finished with them. Dropping the returned future after
/// its first poll therefore does not cancel the write and cannot
/// free memory or close an fd the kernel is still using; the write
/// completes in the background.
///
/// The count is below the buffer length only when the kernel
/// reported zero progress; short writes are resubmitted for the
/// remainder by the driver.
pub(crate) async fn write_at_native(
    ring: &AsyncIoUring,
    file: FileRef,
    buf: IoBuf,
    offset: u64,
) -> Result<usize> {
    ring.submit(|reply| Op::Write {
        file,
        buf,
        offset,
        reply,
    })
    .await
}

/// Submits an `Fsync(DATASYNC)` SQE on `file` (same durability as
/// `fdatasync(2)`). Cancellation behaves as for [`write_at_native`].
pub(crate) async fn fdatasync_native(ring: &AsyncIoUring, file: FileRef) -> Result<()> {
    let _bytes = ring.submit(|reply| Op::Fdatasync { file, reply }).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

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

    fn open_rw(path: &std::path::Path) -> Arc<File> {
        Arc::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .unwrap(),
        )
    }

    fn file_ref(f: &Arc<File>) -> FileRef {
        FileRef::new(Arc::clone(f), |f| f.as_raw_fd())
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
    /// message instead of dragging CI for the job timeout.
    async fn with_timeout<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        const TIMEOUT_SECS: u64 = 15;
        match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), fut).await {
            Ok(v) => v,
            Err(_) => panic!(
                "test exceeded {TIMEOUT_SECS}s timeout, likely a hang in the async substrate"
            ),
        }
    }

    #[tokio::test]
    async fn write_at_native_round_trips() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let path = tmp_path("write");
            let _g = Cleanup(path.clone());
            let f = open_rw(&path);

            let data = vec![0xA5u8; 4096];
            let n = write_at_native(&ring, file_ref(&f), IoBuf::Vec(data.clone()), 0)
                .await
                .expect("write_at_native");
            assert_eq!(n, data.len());
            fdatasync_native(&ring, file_ref(&f))
                .await
                .expect("fdatasync_native");

            drop(f);
            let read_back = std::fs::read(&path).expect("read");
            assert_eq!(read_back, data);
        })
        .await;
    }

    #[tokio::test]
    async fn write_at_invalid_fd_returns_io_error() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            // fd -1 is invalid; kernel returns -EBADF (errno 9).
            let bad = FileRef::new(Arc::new(()), |_| -1);
            let result = write_at_native(&ring, bad, IoBuf::Vec(vec![0u8; 64]), 0).await;
            assert!(matches!(result, Err(Error::Io(_))));
        })
        .await;
    }

    #[tokio::test]
    async fn concurrent_writes_complete_independently() {
        with_timeout(async {
            let Some(ring) = ring_or_skip() else { return };
            let ring = Arc::new(ring);

            let path = tmp_path("concurrent");
            let _g = Cleanup(path.clone());
            // Pre-size the file with 16 sectors of zeros.
            std::fs::write(&path, vec![0u8; 16 * 4096]).unwrap();
            let f = Arc::new(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap(),
            );

            let mut handles = Vec::new();
            for i in 0..16usize {
                let ring = ring.clone();
                let file = file_ref(&f);
                let payload = vec![i as u8; 4096];
                handles.push(tokio::spawn(async move {
                    write_at_native(&ring, file, IoBuf::Vec(payload), (i * 4096) as u64)
                        .await
                        .expect("concurrent write")
                }));
            }
            for h in handles {
                assert_eq!(h.await.unwrap(), 4096);
            }
            fdatasync_native(&ring, file_ref(&f))
                .await
                .expect("fdatasync");
            drop(f);

            let bytes = std::fs::read(&path).unwrap();
            for i in 0..16 {
                let slice = &bytes[i * 4096..(i + 1) * 4096];
                assert!(
                    slice.iter().all(|&b| b == i as u8),
                    "sector {i} content drift, concurrent submission broke ordering"
                );
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

            let mut paths = Vec::with_capacity(N_FDS);
            let mut guards = Vec::with_capacity(N_FDS);
            let mut files = Vec::with_capacity(N_FDS);
            for i in 0..N_FDS {
                let path = tmp_path(&format!("manyfds_{i:02}"));
                guards.push(Cleanup(path.clone()));
                files.push(open_rw(&path));
                paths.push(path);
            }

            for (i, f) in files.iter().enumerate() {
                let payload = vec![i as u8; PAYLOAD_LEN];
                let n = write_at_native(&ring, file_ref(f), IoBuf::Vec(payload), 0)
                    .await
                    .expect("write_at_native");
                assert_eq!(n, PAYLOAD_LEN, "fd {i}: short write");
                fdatasync_native(&ring, file_ref(f))
                    .await
                    .expect("fdatasync_native");
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
            let f = open_rw(&path);
            std::fs::write(&path, vec![0u8; N_WRITES * PAYLOAD_LEN]).unwrap();

            for i in 0..N_WRITES {
                let payload = vec![(i & 0xFF) as u8; PAYLOAD_LEN];
                let n = write_at_native(
                    &ring,
                    file_ref(&f),
                    IoBuf::Vec(payload),
                    (i * PAYLOAD_LEN) as u64,
                )
                .await
                .expect("write_at_native");
                assert_eq!(n, PAYLOAD_LEN, "iter {i}: short write");
            }
            fdatasync_native(&ring, file_ref(&f))
                .await
                .expect("fdatasync_native");
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
            let file_a = open_rw(&path_a);
            let file_b = open_rw(&path_b);
            let fd = file_a.as_raw_fd();

            let payload_a = vec![b'A'; 5000];
            let n = write_at_native(&ring, file_ref(&file_a), IoBuf::Vec(payload_a.clone()), 0)
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
            let n = write_at_native(&ring, file_ref(&file_a), IoBuf::Vec(payload_b.clone()), 0)
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
