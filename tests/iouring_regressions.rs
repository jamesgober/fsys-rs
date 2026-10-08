//! Regression tests for the 1.1.1 io_uring fixes.
//!
//! The tests run on every platform through the public API. On Linux
//! with io_uring available they exercise the per-Handle rings (the
//! sync owner-thread ring behind `Method::Direct` and the native
//! async substrate behind `write_async`); elsewhere they exercise the
//! portable fallbacks and must still pass.
//!
//! Files live under `CARGO_TARGET_TMPDIR` rather than the system temp
//! directory because `/tmp` is tmpfs on many Linux hosts and tmpfs
//! rejects `O_DIRECT`, which would route `Method::Direct` away from
//! the io_uring path these tests target.

use fsys::{builder, Method};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

/// Fresh, empty directory unique to this test invocation.
fn test_dir(tag: &str) -> TempDir {
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "fsys_iouring_regress_{}_{}_{tag}",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    TempDir(dir)
}

/// Asserts `got == want` without dumping kilobytes of bytes on
/// failure: reports lengths and the first differing offset.
fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    if got != want {
        let first_diff = got.iter().zip(want).position(|(g, w)| g != w);
        panic!(
            "{what}: got {} bytes, want {}; first difference at {:?}; head {:?}",
            got.len(),
            want.len(),
            first_diff,
            &got[..got.len().min(8)]
        );
    }
}

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Payload for file `i`: a distinct byte pattern and a distinct,
/// mostly non-sector-multiple length.
fn payload_for(i: usize) -> Vec<u8> {
    let len = 100 + (i * 977) % 9000;
    (0..len)
        .map(|j| (i as u8).wrapping_mul(31) ^ (j as u8))
        .collect()
}

#[test]
fn test_direct_write_three_files_each_keep_own_bytes() {
    // The exact sequence from the FS-C1 report: with the 1.1.0
    // fixed-file cache, a.bin ended up holding c.bin's bytes and the
    // other two files were zero-filled.
    let dir = test_dir("abc");
    let fs = builder()
        .method(Method::Direct)
        .root(&dir.0)
        .build()
        .expect("handle");
    let a = vec![b'A'; 5000];
    let b = vec![b'B'; 3000];
    let c = vec![b'C'; 100];
    fs.write("a.bin", &a).expect("write a");
    fs.write("b.bin", &b).expect("write b");
    fs.write("c.bin", &c).expect("write c");
    for (name, want) in [("a.bin", &a), ("b.bin", &b), ("c.bin", &c)] {
        assert_bytes(&std::fs::read(dir.0.join(name)).unwrap(), want, name);
        assert_bytes(&fs.read(name).unwrap(), want, name);
    }
}

#[test]
fn test_direct_write_many_files_each_keep_own_bytes() {
    let dir = test_dir("many");
    let fs = builder()
        .method(Method::Direct)
        .root(&dir.0)
        .build()
        .expect("handle");
    const FILES: usize = 64;
    for i in 0..FILES {
        fs.write(format!("f{i:03}.bin"), &payload_for(i))
            .expect("write");
    }
    for i in 0..FILES {
        let got = std::fs::read(dir.0.join(format!("f{i:03}.bin"))).unwrap();
        assert_bytes(&got, &payload_for(i), &format!("file {i}"));
    }
}

#[test]
fn test_direct_write_concurrent_fd_churn_each_keep_own_bytes() {
    // Several threads share one Handle (one ring) while temp files
    // open and close constantly, so fd numbers are reused across
    // threads between submissions.
    let dir = test_dir("churn");
    let fs = std::sync::Arc::new(
        builder()
            .method(Method::Direct)
            .root(&dir.0)
            .build()
            .expect("handle"),
    );
    const THREADS: usize = 4;
    const PER_THREAD: usize = 32;
    let mut joins = Vec::new();
    for t in 0..THREADS {
        let fs = fs.clone();
        joins.push(std::thread::spawn(move || {
            for k in 0..PER_THREAD {
                let i = t * PER_THREAD + k;
                fs.write(format!("t{i:03}.bin"), &payload_for(i))
                    .expect("write");
            }
        }));
    }
    for j in joins {
        j.join().expect("writer thread");
    }
    for i in 0..THREADS * PER_THREAD {
        let got = std::fs::read(dir.0.join(format!("t{i:03}.bin"))).unwrap();
        assert_bytes(&got, &payload_for(i), &format!("file {i}"));
    }
}

#[cfg(feature = "async")]
mod async_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_direct_write_async_many_files_each_keep_own_bytes() {
        let dir = test_dir("async_many");
        let fs = Arc::new(
            builder()
                .method(Method::Direct)
                .root(&dir.0)
                .build()
                .expect("handle"),
        );
        const FILES: usize = 48;
        for i in 0..FILES {
            fs.clone()
                .write_async(format!("f{i:03}.bin"), payload_for(i))
                .await
                .expect("write_async");
        }
        let mut joins = Vec::new();
        for i in FILES..FILES * 2 {
            let fs = fs.clone();
            joins.push(tokio::spawn(async move {
                fs.write_async(format!("f{i:03}.bin"), payload_for(i)).await
            }));
        }
        for j in joins {
            j.await.expect("join").expect("write_async");
        }
        for i in 0..FILES * 2 {
            let got = std::fs::read(dir.0.join(format!("f{i:03}.bin"))).unwrap();
            assert_bytes(&got, &payload_for(i), &format!("file {i}"));
        }
    }

    /// Reads every record in the journal at `path` if the file has a
    /// clean tail, `None` otherwise.
    fn read_journal(path: &std::path::Path) -> Option<Vec<Vec<u8>>> {
        let mut reader = fsys::JournalReader::open(path).ok()?;
        let mut out = Vec::new();
        for record in reader.iter() {
            out.push(record.ok()?.payload);
        }
        (reader.tail_state() == fsys::JournalTailState::CleanEnd).then_some(out)
    }

    /// FS-C2: dropping `append_async` futures right after their first
    /// poll must neither free the frame under the kernel nor leave a
    /// hole in the reserved LSN range. Every record must eventually
    /// decode with a valid CRC, in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_cancelled_journal_appends_leave_intact_records() {
        let dir = test_dir("journal_cancel");
        let fs = builder().root(&dir.0).build().expect("handle");
        let path = dir.0.join("cancel.wal");
        let log = Arc::new(fs.journal(&path).expect("journal"));
        let mut expected = Vec::new();
        for i in 0..200 {
            let record = format!("record-{i:04}-{}", "x".repeat(i % 97)).into_bytes();
            let fut = log.clone().append_async(record.clone());
            // `timeout` polls the inner future once before checking
            // the zero deadline, so the append is queued and then
            // dropped.
            let _elapsed = tokio::time::timeout(Duration::ZERO, fut).await;
            // Reuse freed memory right away so a dangling SQE would
            // pick up foreign bytes.
            drop(std::hint::black_box(vec![0xEEu8; record.len() + 12]));
            expected.push(record);
        }
        let tail = log
            .clone()
            .append_async(b"tail".to_vec())
            .await
            .expect("tail append");
        log.clone().sync_through_async(tail).await.expect("sync");
        expected.push(b"tail".to_vec());

        // The dropped appends finish in the background; give them a
        // bounded window to land.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match read_journal(&path) {
                Some(records) if records == expected => break,
                other => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "journal never settled: {:?} of {} records readable",
                        other.map(|r| r.len()),
                        expected.len()
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }

    /// FS-C2: cancelling `write_async` at an early await point must
    /// leave each target with either its previous content or the full
    /// new payload, never foreign bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_cancelled_write_async_keeps_old_or_new_content() {
        let dir = test_dir("write_cancel");
        let fs = Arc::new(
            builder()
                .method(Method::Direct)
                .root(&dir.0)
                .build()
                .expect("handle"),
        );
        const FILES: usize = 40;
        for i in 0..FILES {
            fs.write(format!("c{i:03}.bin"), b"old").expect("seed");
        }
        for i in 0..FILES {
            let fut = fs
                .clone()
                .write_async(format!("c{i:03}.bin"), payload_for(i));
            let budget = Duration::from_micros((i as u64 % 8) * 40);
            let _elapsed = tokio::time::timeout(budget, fut).await;
            drop(std::hint::black_box(vec![0xEEu8; payload_for(i).len()]));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        for i in 0..FILES {
            let got = std::fs::read(dir.0.join(format!("c{i:03}.bin"))).unwrap();
            if got != b"old" {
                assert_bytes(&got, &payload_for(i), &format!("file {i}"));
            }
        }
    }
}
