//! Resume scan and corrupt-tail handling for journal open (1.1.3).
//!
//! Opening a journal scans it forward with
//! [`JournalReader`](super::JournalReader) and resumes appending at
//! the end of the last valid frame (the clean end). Whatever lies
//! between the clean end and the end of the file is unreachable by
//! the reader, whatever stopped the scan: a torn frame, a zero run,
//! a CRC mismatch, a bad magic or an oversized length field. The
//! open cuts it off so new appends land where the reader can reach
//! them, never behind garbage.
//!
//! Before anything is cut off:
//!
//! - A tail of only zero bytes (Direct-IO sector padding, space a
//!   preallocation fallback zero-filled, a reservation that was
//!   never written) carries nothing and is dropped as it is.
//! - Any other tail is copied to a sidecar file in the journal's
//!   directory named `<journal file name>.corrupt-<clean end>`, the
//!   clean end written in decimal. The copy is synced and the
//!   directory entry is synced before [`prepare_resume`] returns,
//!   and the caller truncates only after that, so a crash between
//!   the copy and the truncate leaves both the journal and the copy
//!   intact. If the copy cannot be made the open fails and the
//!   journal is not touched.
//!
//! A sidecar name that is already taken by a file with different
//! contents gets `.1`, `.2`, ... appended. A taken name whose file
//! holds exactly the tail bytes is reused: that is the state a crash
//! between the copy and the truncate leaves behind, and reusing it
//! keeps the reopen after such a crash from making a second copy.

use super::reader::{JournalReader, JournalTailState};
use crate::{Error, Result};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Read size for the zero check, the copy and the comparison.
/// The tail is read only on the open path, never while appending.
const CHUNK: usize = 64 * 1024;

/// Where a journal open resumes, and what it found past that point.
#[derive(Debug)]
pub(super) struct Resume {
    /// End of the last valid frame: the LSN appends resume at.
    pub(super) clean_end: u64,
    /// Bytes from `clean_end` to the end of the file.
    pub(super) tail: Tail,
}

impl Resume {
    /// The resume point of a file that does not exist yet.
    pub(super) const fn empty() -> Self {
        Self {
            clean_end: 0,
            tail: Tail::Empty,
        }
    }

    /// `true` when the tail holds a non-zero byte, so it was copied
    /// to a sidecar and must not survive the open.
    pub(super) fn tail_has_data(&self) -> bool {
        matches!(self.tail, Tail::Preserved(_))
    }
}

/// Contents of `[clean_end, file_len)`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Tail {
    /// The file ends at the clean end.
    Empty,
    /// Only zero bytes follow the clean end.
    Zeros,
    /// At least one non-zero byte follows the clean end. The whole
    /// tail is durable in the sidecar file at this path.
    Preserved(PathBuf),
}

/// Scans `path` for its clean end and saves any non-zero tail to a
/// sidecar file, durably, so the caller may truncate to
/// [`Resume::clean_end`].
///
/// # Errors
///
/// [`Error::Io`] when the journal cannot be read, or when a
/// non-zero tail cannot be saved (the sidecar cannot be created,
/// written or synced, or the directory cannot be synced). The
/// journal file is not modified in either case.
pub(super) fn prepare_resume(path: &Path) -> Result<Resume> {
    let (clean_end, file_len, state) = scan_clean_end(path)?;
    let tail = preserve_tail(path, clean_end, file_len)?;
    report(path, clean_end, file_len, state, &tail);
    Ok(Resume { clean_end, tail })
}

/// Returns `(clean_end, file_len, tail_state)`: the byte offset just
/// past the last cleanly decoded frame, the file size the reader saw
/// and why the scan stopped. `(0, 0, CleanEnd)` for an empty file.
///
/// Every tail state resumes at the clean end. Before 1.1.3 a bad
/// magic or an oversized length field failed the open instead.
fn scan_clean_end(path: &Path) -> Result<(u64, u64, JournalTailState)> {
    let mut reader = JournalReader::open(path)?;
    let file_len = reader.file_size();
    if file_len == 0 {
        return Ok((0, 0, JournalTailState::CleanEnd));
    }
    let mut iter = reader.iter();
    while iter.next().transpose()?.is_some() {}
    drop(iter);
    Ok((reader.position().0, file_len, reader.tail_state()))
}

/// Classifies `[clean_end, file_len)` and copies it to a sidecar
/// when it holds any non-zero byte.
fn preserve_tail(path: &Path, clean_end: u64, file_len: u64) -> Result<Tail> {
    if clean_end >= file_len {
        return Ok(Tail::Empty);
    }
    let len = file_len - clean_end;
    let mut buf = vec![0u8; CHUNK];
    let zeros = tail_is_zero(path, clean_end, len, &mut buf)
        .map_err(|e| tail_error(path, clean_end, len, None, e))?;
    if zeros {
        return Ok(Tail::Zeros);
    }
    write_sidecar(path, clean_end, len, &mut buf).map(Tail::Preserved)
}

/// Emits the tracing event for a tail the open is about to drop.
#[cfg(feature = "tracing")]
fn report(path: &Path, clean_end: u64, file_len: u64, state: JournalTailState, tail: &Tail) {
    let discarded = file_len.saturating_sub(clean_end);
    match tail {
        Tail::Empty => {}
        Tail::Zeros => tracing::debug!(
            path = ?path,
            clean_end,
            discarded,
            "journal open found only zero bytes after the last valid frame"
        ),
        Tail::Preserved(sidecar) => tracing::warn!(
            path = ?path,
            clean_end,
            discarded,
            tail_state = ?state,
            sidecar = ?sidecar,
            "journal open found unreadable bytes after the last valid frame; \
             saved them to the sidecar file and truncated the journal"
        ),
    }
}

/// Without the `tracing` feature there is no event to emit.
#[cfg(not(feature = "tracing"))]
#[inline]
fn report(_path: &Path, _clean_end: u64, _file_len: u64, _state: JournalTailState, _tail: &Tail) {}

/// Opens `path` for reading, positioned at `offset`, limited to
/// `len` bytes. A plain buffered handle: the journal's own handle
/// may be a Direct-IO one, which needs aligned reads.
fn open_range(path: &Path, offset: u64, len: u64) -> std::io::Result<std::io::Take<File>> {
    let mut file = File::open(path)?;
    let _ = file.seek(SeekFrom::Start(offset))?;
    Ok(file.take(len))
}

/// `read` that retries on `Interrupted`.
fn read_some(src: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match src.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// `true` when every byte of `[offset, offset + len)` is zero.
fn tail_is_zero(path: &Path, offset: u64, len: u64, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut src = open_range(path, offset, len)?;
    loop {
        let n = read_some(&mut src, buf)?;
        if n == 0 {
            return Ok(true);
        }
        if buf[..n].iter().any(|&b| b != 0) {
            return Ok(false);
        }
    }
}

/// Sidecar path for a tail starting at `clean_end`. Attempt `0` is
/// `<name>.corrupt-<clean_end>`; attempt `n > 0` appends `.<n>`.
fn sidecar_path(path: &Path, clean_end: u64, attempt: u32) -> std::io::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "journal path has no file name",
        )
    })?;
    let mut name = OsString::from(file_name);
    name.push(format!(".corrupt-{clean_end}"));
    if attempt > 0 {
        name.push(format!(".{attempt}"));
    }
    Ok(path.with_file_name(name))
}

/// Copies `[clean_end, clean_end + len)` of `path` to a new sidecar
/// file, syncs it and its directory, and returns its path. Reuses an
/// existing sidecar that already holds exactly these bytes.
fn write_sidecar(path: &Path, clean_end: u64, len: u64, buf: &mut [u8]) -> Result<PathBuf> {
    let mut attempt: u32 = 0;
    loop {
        let candidate = sidecar_path(path, clean_end, attempt)
            .map_err(|e| tail_error(path, clean_end, len, None, e))?;
        let fail = |e| tail_error(path, clean_end, len, Some(&candidate), e);
        #[cfg(test)]
        fault::sidecar_create().map_err(fail)?;
        match new_sidecar_options().open(&candidate) {
            Ok(mut file) => {
                if let Err(e) = fill_sidecar(&mut file, path, clean_end, len, buf) {
                    drop(file);
                    // Remove the partial copy this call created so it
                    // is not mistaken for a full one. The result is
                    // ignored: the open fails with the copy error
                    // either way, the journal is untouched, and a
                    // leftover partial file only makes the next
                    // attempt pick the next suffix.
                    let _ = std::fs::remove_file(&candidate);
                    return Err(fail(e));
                }
                drop(file);
                sync_dir(&candidate).map_err(fail)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(existing) = matching_sidecar(&candidate, path, clean_end, len, buf) {
                    existing.sync_all().map_err(fail)?;
                    drop(existing);
                    sync_dir(&candidate).map_err(fail)?;
                    return Ok(candidate);
                }
                attempt = attempt.checked_add(1).ok_or_else(|| {
                    fail(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "every sidecar file name is taken",
                    ))
                })?;
            }
            Err(e) => return Err(fail(e)),
        }
    }
}

/// Options for creating a sidecar: never overwrite an existing file.
///
/// The tail may hold record data, so on Unix the sidecar is created
/// owner-only (`0o600`), never wider than whatever mode the journal
/// has. On Windows it inherits the directory's ACL, as the journal
/// file does.
fn new_sidecar_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = options.mode(0o600);
    }
    options
}

/// Syncs the directory holding `sidecar` so its entry is durable,
/// as an IO error the caller can wrap with context.
fn sync_dir(sidecar: &Path) -> std::io::Result<()> {
    match crate::platform::sync_parent_dir(sidecar) {
        Ok(()) => Ok(()),
        Err(Error::Io(e)) => Err(e),
        Err(other) => Err(std::io::Error::other(other.to_string())),
    }
}

/// Writes the tail into `file` and syncs it.
fn fill_sidecar(
    file: &mut File,
    path: &Path,
    clean_end: u64,
    len: u64,
    buf: &mut [u8],
) -> std::io::Result<()> {
    use std::io::Write;
    let mut src = open_range(path, clean_end, len)?;
    let mut copied: u64 = 0;
    loop {
        let n = read_some(&mut src, buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        copied += n as u64;
    }
    if copied != len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("journal shrank while its tail was copied ({copied} of {len} bytes)"),
        ));
    }
    file.sync_all()
}

/// Returns a writable handle to `candidate` when it holds exactly
/// the `len` tail bytes at `clean_end` of `path`; `None` otherwise.
///
/// Any error opening or reading either file counts as a mismatch:
/// the caller then writes a fresh copy under the next name, which
/// never loses the tail.
fn matching_sidecar(
    candidate: &Path,
    path: &Path,
    clean_end: u64,
    len: u64,
    buf: &mut [u8],
) -> Option<File> {
    // Write access so `sync_all` works on every platform (Windows
    // `FlushFileBuffers` needs it); a sidecar the open cannot sync
    // is not reused.
    let existing = OpenOptions::new()
        .read(true)
        .write(true)
        .open(candidate)
        .ok()?;
    if existing.metadata().ok()?.len() != len {
        return None;
    }
    let mut ours = open_range(path, clean_end, len).ok()?;
    let mut theirs = (&existing).take(len);
    let (a, b) = buf.split_at_mut(buf.len() / 2);
    let mut left = len;
    while left > 0 {
        let n = usize::try_from(left).map_or(a.len(), |l| l.min(a.len()));
        ours.read_exact(&mut a[..n]).ok()?;
        theirs.read_exact(&mut b[..n]).ok()?;
        if a[..n] != b[..n] {
            return None;
        }
        left -= n as u64;
    }
    Some(existing)
}

/// Wraps an IO error from saving the tail with what the open was
/// doing, keeping the original error kind.
fn tail_error(
    path: &Path,
    clean_end: u64,
    len: u64,
    sidecar: Option<&Path>,
    e: std::io::Error,
) -> Error {
    let target = match sidecar {
        Some(s) => format!(" to {s:?}"),
        None => String::new(),
    };
    Error::Io(std::io::Error::new(
        e.kind(),
        format!(
            "journal at {path:?} has {len} unreadable bytes after its last valid frame \
             at offset {clean_end}; saving them{target} before truncating failed, \
             so the journal was not opened or changed: {e}"
        ),
    ))
}

/// Test-only fault injection for the open path. Thread-local, so a
/// test only affects opens on its own thread.
#[cfg(test)]
pub(super) mod fault {
    use std::cell::Cell;

    thread_local! {
        static FAIL_SIDECAR: Cell<bool> = const { Cell::new(false) };
        static STOP_BEFORE_TRUNCATE: Cell<bool> = const { Cell::new(false) };
    }

    /// Makes every sidecar creation on this thread fail.
    pub(crate) fn fail_sidecar(on: bool) {
        FAIL_SIDECAR.with(|c| c.set(on));
    }

    /// Makes every open on this thread return an error after the
    /// sidecar is durable and before the journal is truncated: the
    /// on-disk state a crash at that point leaves.
    pub(crate) fn stop_before_truncate(on: bool) {
        STOP_BEFORE_TRUNCATE.with(|c| c.set(on));
    }

    pub(super) fn sidecar_create() -> std::io::Result<()> {
        if FAIL_SIDECAR.with(Cell::get) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected sidecar creation failure",
            ));
        }
        Ok(())
    }

    pub(crate) fn before_truncate() -> crate::Result<()> {
        if STOP_BEFORE_TRUNCATE.with(Cell::get) {
            return Err(crate::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "injected stop before truncate",
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static C: AtomicU64 = AtomicU64::new(0);

    /// A fresh directory per test, removed on drop.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new(tag: &str) -> Self {
            let n = C.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "fsys_tail_test_{}_{}_{tag}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&dir).expect("create test dir");
            Self(dir)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
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

    #[test]
    fn test_sidecar_path_appends_offset_and_attempt() {
        let p = Path::new("dir").join("wal.log");
        assert_eq!(
            sidecar_path(&p, 1234, 0).expect("name"),
            Path::new("dir").join("wal.log.corrupt-1234")
        );
        assert_eq!(
            sidecar_path(&p, 0, 2).expect("name"),
            Path::new("dir").join("wal.log.corrupt-0.2")
        );
    }

    #[test]
    fn test_sidecar_path_without_file_name_is_error() {
        assert!(sidecar_path(Path::new(".."), 0, 0).is_err());
    }

    #[test]
    fn test_preserve_tail_empty_and_zero_tails_make_no_sidecar() {
        let dir = TestDir::new("zero");
        let p = dir.file("j");
        std::fs::write(&p, [1u8, 2, 3, 0, 0, 0, 0]).expect("write");
        assert_eq!(preserve_tail(&p, 7, 7).expect("empty"), Tail::Empty);
        assert_eq!(preserve_tail(&p, 3, 7).expect("zeros"), Tail::Zeros);
        assert_eq!(dir.names(), vec!["j".to_string()]);
    }

    #[test]
    fn test_preserve_tail_copies_exact_bytes() {
        let dir = TestDir::new("copy");
        let p = dir.file("j");
        // Zeros before a non-zero byte still count as data: the
        // whole range is copied.
        let mut bytes = vec![7u8; 10];
        bytes.extend_from_slice(&[0, 0, 9]);
        std::fs::write(&p, &bytes).expect("write");
        let tail = preserve_tail(&p, 10, 13).expect("preserve");
        let side = dir.file("j.corrupt-10");
        assert_eq!(tail, Tail::Preserved(side.clone()));
        assert_eq!(std::fs::read(&side).expect("read"), vec![0, 0, 9]);
        assert_eq!(std::fs::read(&p).expect("read"), bytes, "journal untouched");
    }

    #[test]
    fn test_preserve_tail_tail_larger_than_chunk() {
        let dir = TestDir::new("large");
        let p = dir.file("j");
        let tail: Vec<u8> = (0..(CHUNK * 2 + 17)).map(|i| (i % 251) as u8 | 1).collect();
        std::fs::write(&p, &tail).expect("write");
        let len = tail.len() as u64;
        let side = match preserve_tail(&p, 0, len).expect("preserve") {
            Tail::Preserved(s) => s,
            other => panic!("expected a sidecar, got {other:?}"),
        };
        assert_eq!(std::fs::read(&side).expect("read"), tail);
        // Same bytes again: the sidecar is reused, not duplicated.
        assert_eq!(
            preserve_tail(&p, 0, len).expect("again"),
            Tail::Preserved(side)
        );
        assert_eq!(dir.names().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn test_preserve_tail_sidecar_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("mode");
        let p = dir.file("j");
        std::fs::write(&p, [1u8, 2, 3]).expect("write");
        let side = match preserve_tail(&p, 1, 3).expect("preserve") {
            Tail::Preserved(s) => s,
            other => panic!("expected a sidecar, got {other:?}"),
        };
        let mode = std::fs::metadata(&side).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn test_preserve_tail_collision_with_other_bytes_uses_next_suffix() {
        let dir = TestDir::new("collide");
        let p = dir.file("j");
        std::fs::write(&p, [5u8, 6, 7]).expect("write");
        std::fs::write(dir.file("j.corrupt-1"), b"xy").expect("occupy");
        std::fs::write(dir.file("j.corrupt-1.1"), b"zz").expect("occupy");
        let tail = preserve_tail(&p, 1, 3).expect("preserve");
        assert_eq!(tail, Tail::Preserved(dir.file("j.corrupt-1.2")));
        assert_eq!(std::fs::read(dir.file("j.corrupt-1")).expect("r"), b"xy");
        assert_eq!(std::fs::read(dir.file("j.corrupt-1.2")).expect("r"), [6, 7]);
    }

    #[test]
    fn test_preserve_tail_sidecar_failure_is_error_and_leaves_journal() {
        let dir = TestDir::new("fail");
        let p = dir.file("j");
        std::fs::write(&p, [1u8, 2, 3]).expect("write");
        fault::fail_sidecar(true);
        let r = preserve_tail(&p, 1, 3);
        fault::fail_sidecar(false);
        match r {
            Err(Error::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
                assert!(e.to_string().contains("j.corrupt-1"), "{e}");
            }
            other => panic!("expected an Io error, got {other:?}"),
        }
        assert_eq!(dir.names(), vec!["j".to_string()]);
        assert_eq!(std::fs::read(&p).expect("read"), [1, 2, 3]);
    }

    #[test]
    fn test_prepare_resume_missing_file_is_error() {
        let dir = TestDir::new("missing");
        assert!(prepare_resume(&dir.file("absent")).is_err());
    }

    #[test]
    fn test_resume_empty_has_no_data() {
        let r = Resume::empty();
        assert_eq!(r.clean_end, 0);
        assert!(!r.tail_has_data());
    }
}
