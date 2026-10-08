//! Corrupt-tail tolerance on journal open (1.1.3).
//!
//! fsys 1.1.1 and 1.1.2 refused to open a journal whose scan stopped
//! at a bad magic or an oversized length field, which broke
//! consumers that recover the valid prefix of a journal followed by
//! garbage. Since 1.1.3 every tail state opens at the end of the last
//! valid frame; a discarded tail that is not all zero bytes is first
//! copied to `<name>.corrupt-<clean end>` next to the journal.
//!
//! Every test runs in buffered and Direct-IO mode. On filesystems
//! that reject Direct IO, `direct(true)` falls back to buffered IO
//! and the Direct-IO runs exercise the buffered path.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use fsys::{builder, JournalOptions, JournalReader, JournalTailState};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

/// A fresh directory per test case, removed on drop, so sidecar
/// files can be listed and never leak into the shared temp dir.
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let n = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fsys_corrupt_tail_{}_{}_{tag}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        Self(dir)
    }

    fn journal(&self) -> PathBuf {
        self.0.join("wal")
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn opts(direct: bool) -> JournalOptions {
    JournalOptions::new().direct(direct)
}

/// Writes `records` through the public API and returns the clean
/// end (the LSN after the last record). Direct-IO journals are
/// sector-padded on disk; the image helpers below cut that off.
fn write_records(path: &Path, direct: bool, records: &[Vec<u8>]) -> u64 {
    let fs = builder().build().expect("handle");
    let log = fs.journal_with(path, opts(direct)).expect("open");
    for r in records {
        let _ = log.append(r).expect("append");
    }
    let end = log.next_lsn().as_u64();
    log.close().expect("close");
    end
}

/// Encoded frames for `records`, as a buffered journal writes them.
fn frames(records: &[Vec<u8>]) -> Vec<u8> {
    let dir = TestDir::new("frames");
    let path = dir.journal();
    let _ = write_records(&path, false, records);
    std::fs::read(&path).expect("read frames")
}

fn numbered(n: usize, tag: &str) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("{tag}-{i:03}").into_bytes())
        .collect()
}

/// Rewrites the journal as its first `end` bytes followed by `tail`.
fn replace_tail(path: &Path, end: u64, tail: &[u8]) {
    let mut bytes = std::fs::read(path).expect("read");
    bytes.truncate(end as usize);
    bytes.extend_from_slice(tail);
    std::fs::write(path, &bytes).expect("write corrupt image");
}

fn read_all(path: &Path) -> (Vec<Vec<u8>>, JournalTailState, u64) {
    let mut reader = JournalReader::open(path).expect("reader");
    let records = reader.iter().map(|r| r.expect("record").payload).collect();
    (records, reader.tail_state(), reader.position().as_u64())
}

/// Deterministic pseudo-random bytes whose first byte cannot start a
/// frame or a zero run, so the reader stops with `BadMagic`.
fn garbage(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out: Vec<u8> = (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    out[0] = 0xDE;
    out
}

/// Opens a journal holding `prefix` followed by `tail` and checks the
/// common contract: the open resumes at the end of the prefix, the
/// tail lands byte-for-byte in `<name>.corrupt-<end>`, the prefix
/// stays readable, and appends after the open are readable after a
/// reopen that creates no further sidecar.
fn check_tail_is_saved_and_cut(tag: &str, tail: &[u8], expect_state: JournalTailState) {
    for direct in [false, true] {
        let dir = TestDir::new(tag);
        let path = dir.journal();
        let prefix = numbered(50, "rec");
        let end = write_records(&path, direct, &prefix);
        replace_tail(&path, end, tail);
        let (_, state, pos) = read_all(&path);
        assert_eq!(state, expect_state, "{tag} direct={direct}");
        assert_eq!(pos, end, "{tag} direct={direct}");

        let fs = builder().build().expect("handle");
        let log = fs.journal_with(&path, opts(direct)).expect("open");
        assert_eq!(log.next_lsn().as_u64(), end, "{tag} direct={direct}");
        let side = format!("wal.corrupt-{end}");
        assert_eq!(dir.names(), vec!["wal".to_string(), side.clone()]);
        assert_eq!(
            std::fs::read(dir.0.join(&side)).expect("sidecar"),
            tail,
            "{tag} direct={direct}: sidecar must hold exactly the tail"
        );
        let added = numbered(5, "new");
        for r in &added {
            let _ = log.append(r).expect("append");
        }
        log.close().expect("close");

        let log = fs.journal_with(&path, opts(direct)).expect("reopen");
        drop(log);
        assert_eq!(dir.names(), vec!["wal".to_string(), side]);
        let (records, state, _) = read_all(&path);
        let mut want = prefix;
        want.extend(added);
        assert_eq!(records, want, "{tag} direct={direct}");
        assert_eq!(state, JournalTailState::CleanEnd, "{tag} direct={direct}");
    }
}

/// The emdb case: random bytes after a valid prefix (bad magic).
#[test]
fn test_open_random_garbage_tail_recovers_prefix_and_saves_garbage() {
    check_tail_is_saved_and_cut("garbage", &garbage(256, 42), JournalTailState::BadMagic);
}

#[test]
fn test_open_checksum_mismatch_tail_is_saved_and_cut() {
    let mut torn = frames(&[b"torn-frame-payload".to_vec()]);
    let last = torn.len() - 1;
    torn[last] ^= 0xFF;
    check_tail_is_saved_and_cut("crc", &torn, JournalTailState::ChecksumMismatch);
}

#[test]
fn test_open_length_overflow_tail_is_saved_and_cut() {
    let mut tail = frames(&[b"x".to_vec()]);
    // Keep the magic, declare a length over the 256 MiB cap.
    tail[4..8].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    check_tail_is_saved_and_cut("overflow", &tail, JournalTailState::LengthOverflow);
}

#[test]
fn test_open_torn_header_tail_is_saved_and_cut() {
    let tail = frames(&[b"header".to_vec()])[..5].to_vec();
    check_tail_is_saved_and_cut("torn_header", &tail, JournalTailState::TruncatedHeader);
}

#[test]
fn test_open_partial_frame_tail_is_saved_and_cut() {
    let full = frames(&[b"partial-frame-payload".to_vec()]);
    let tail = full[..full.len() - 3].to_vec();
    check_tail_is_saved_and_cut("partial", &tail, JournalTailState::TruncatedPayload);
}

/// Mid-log corruption followed by later valid frames is a corrupt
/// tail: the records after the bad frame were already unreachable
/// by the reader, and they are kept in the sidecar.
#[test]
fn test_open_mid_log_corruption_keeps_later_frames_in_sidecar() {
    let mut tail = frames(&[b"bad".to_vec(), b"later-1".to_vec(), b"later-2".to_vec()]);
    tail[9] ^= 0x01; // a payload byte of the first frame
    check_tail_is_saved_and_cut("mid_log", &tail, JournalTailState::ChecksumMismatch);
}

/// Zero bytes after the last record are padding, not data: they
/// are dropped without a sidecar.
#[test]
fn test_open_all_zero_tail_truncates_without_sidecar() {
    for direct in [false, true] {
        let dir = TestDir::new("zeros");
        let path = dir.journal();
        let prefix = numbered(10, "rec");
        let end = write_records(&path, direct, &prefix);
        replace_tail(&path, end, &vec![0u8; 3 * 4096 + 7]);

        let fs = builder().build().expect("handle");
        let log = fs.journal_with(&path, opts(direct)).expect("open");
        assert_eq!(log.next_lsn().as_u64(), end, "direct={direct}");
        let _ = log.append(b"after").expect("append");
        log.close().expect("close");
        assert_eq!(dir.names(), vec!["wal".to_string()], "direct={direct}");
        let (records, state, _) = read_all(&path);
        let mut want = prefix;
        want.push(b"after".to_vec());
        assert_eq!(records, want);
        assert_eq!(state, JournalTailState::CleanEnd);
    }
}

/// A file that is not a journal at all opens empty; its whole
/// content is kept in `<name>.corrupt-0`.
#[test]
fn test_open_garbage_only_file_moves_everything_to_sidecar() {
    for direct in [false, true] {
        let dir = TestDir::new("all_garbage");
        let path = dir.journal();
        let content = b"\xDE\xAD\xBE\xEF\x00\x00\x00\x00garbage".to_vec();
        std::fs::write(&path, &content).expect("write");

        let fs = builder().build().expect("handle");
        let log = fs.journal_with(&path, opts(direct)).expect("open");
        assert_eq!(log.next_lsn().as_u64(), 0, "direct={direct}");
        let _ = log.append(b"first").expect("append");
        log.close().expect("close");
        assert_eq!(
            dir.names(),
            vec!["wal".to_string(), "wal.corrupt-0".to_string()]
        );
        assert_eq!(
            std::fs::read(dir.0.join("wal.corrupt-0")).expect("sidecar"),
            content
        );
        assert_eq!(read_all(&path).0, vec![b"first".to_vec()]);
    }
}

/// A sidecar name taken by a file with other contents gets a `.1`
/// suffix; the existing file is left alone.
#[test]
fn test_open_sidecar_name_collision_uses_next_suffix() {
    for direct in [false, true] {
        let dir = TestDir::new("collision");
        let path = dir.journal();
        let end = write_records(&path, direct, &numbered(3, "rec"));
        let tail = garbage(100, 7);
        replace_tail(&path, end, &tail);
        let taken = dir.0.join(format!("wal.corrupt-{end}"));
        std::fs::write(&taken, b"an earlier, different tail").expect("occupy");

        let fs = builder().build().expect("handle");
        let log = fs.journal_with(&path, opts(direct)).expect("open");
        assert_eq!(log.next_lsn().as_u64(), end);
        drop(log);
        assert_eq!(
            dir.names(),
            vec![
                "wal".to_string(),
                format!("wal.corrupt-{end}"),
                format!("wal.corrupt-{end}.1"),
            ]
        );
        assert_eq!(
            std::fs::read(&taken).expect("old"),
            b"an earlier, different tail"
        );
        assert_eq!(
            std::fs::read(dir.0.join(format!("wal.corrupt-{end}.1"))).expect("new"),
            tail
        );
    }
}

/// The state a crash between the sidecar sync and the truncate
/// leaves: an identical sidecar already exists and the journal still
/// holds the tail. The reopen reuses the sidecar instead of writing
/// a second copy, and later reopens find nothing to save.
#[test]
fn test_open_after_crash_before_truncate_reuses_identical_sidecar() {
    for direct in [false, true] {
        let dir = TestDir::new("crash_state");
        let path = dir.journal();
        let end = write_records(&path, direct, &numbered(4, "rec"));
        let tail = garbage(700, 99);
        replace_tail(&path, end, &tail);
        let side = format!("wal.corrupt-{end}");
        std::fs::write(dir.0.join(&side), &tail).expect("pre-crash sidecar");

        let fs = builder().build().expect("handle");
        for _ in 0..3 {
            let log = fs.journal_with(&path, opts(direct)).expect("open");
            assert_eq!(log.next_lsn().as_u64(), end, "direct={direct}");
            drop(log);
            assert_eq!(dir.names(), vec!["wal".to_string(), side.clone()]);
        }
        assert_eq!(std::fs::read(dir.0.join(&side)).expect("sidecar"), tail);
    }
}

/// When the sidecar cannot be created the open fails and the journal
/// is left byte-for-byte unchanged. Unix only: it needs a directory
/// the process cannot create files in, and Windows directory
/// attributes do not prevent file creation. The fault-injection unit
/// test in `src/journal` covers the same path on every platform.
#[cfg(unix)]
#[test]
fn test_open_fails_without_truncating_when_sidecar_cannot_be_created() {
    use std::os::unix::fs::PermissionsExt;
    for direct in [false, true] {
        let dir = TestDir::new("read_only");
        let path = dir.journal();
        let end = write_records(&path, direct, &numbered(3, "rec"));
        replace_tail(&path, end, &garbage(64, 3));
        let before = std::fs::read(&path).expect("read");

        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o555))
            .expect("make dir read-only");
        let probe = dir.0.join("probe");
        if std::fs::File::create(&probe).is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755))
                .expect("restore");
            eprintln!(
                "skipping: directory permissions are not enforced for this user \
                 (running as root?)"
            );
            return;
        }

        let fs = builder().build().expect("handle");
        let result = fs.journal_with(&path, opts(direct));
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).expect("restore");
        match result {
            Err(fsys::Error::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
                assert!(e.to_string().contains("corrupt"), "{e}");
            }
            Err(other) => panic!("unexpected error {other:?}"),
            Ok(_) => panic!("opened without saving the tail, direct={direct}"),
        }
        assert_eq!(std::fs::read(&path).expect("read"), before);
        assert_eq!(dir.names(), vec!["wal".to_string()]);
    }
}
