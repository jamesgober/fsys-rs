//! Fallback IO primitives for unknown/unsupported platforms.
//!
//! Uses only `std::fs` and `std::io` — no Direct IO, no platform syscalls.
//! `probe_direct_io_available()` returns `false` so the method resolver
//! always selects `Method::Sync` on these targets.
//!
//! Positioned IO (`write_at`, `read_range`) never moves a shared file
//! cursor on Unix-family targets (the BSDs and others): it goes through
//! `std::os::unix::fs::FileExt`, which is `pwrite(2)` / `pread(2)`. On
//! non-Unix targets the only portable primitive is seek-then-IO, so those
//! calls are serialised through a process-wide lock and restore the
//! cursor they found.

#![cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]

use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

// ──────────────────────────────────────────────────────────────────────────────
// File opening
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn open_write_new(path: &Path, _use_direct: bool) -> Result<(File, bool)> {
    let f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(Error::Io)?;
    Ok((f, false))
}

pub(crate) fn open_read(path: &Path, _use_direct: bool) -> Result<(File, bool)> {
    let f = File::open(path).map_err(Error::Io)?;
    Ok((f, false))
}

pub(crate) fn open_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(Error::Io)
}

pub(crate) fn open_write_at(path: &Path) -> Result<File> {
    // `truncate(false)`: random-access writes must keep the rest of the
    // file.
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Writing
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn write_all(file: &File, data: &[u8]) -> Result<()> {
    (&*file).write_all(data).map_err(Error::Io)
}

pub(crate) fn write_all_direct(file: &File, data: &[u8], _sector_size: u32) -> Result<()> {
    // Direct IO not available; fall through to buffered write.
    write_all(file, data)
}

pub(crate) fn write_at(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    positioned::write_all_at(file, offset, data).map_err(Error::Io)
}

/// Sector-aligned positioned write — no Direct IO on unknown platforms;
/// delegates to the buffered [`write_at`].
pub(crate) fn write_at_direct(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    write_at(file, offset, data)
}

// ──────────────────────────────────────────────────────────────────────────────
// Reading
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn read_all(file: &File) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let _bytes = (&*file).read_to_end(&mut buf).map_err(Error::Io)?;
    Ok(buf)
}

pub(crate) fn read_all_direct(file: &File, file_size: u64, _sector_size: u32) -> Result<Vec<u8>> {
    // No Direct IO; read normally and trim to file_size.
    let mut buf = Vec::new();
    let _bytes = (&*file).read_to_end(&mut buf).map_err(Error::Io)?;
    buf.truncate(usize::try_from(file_size).unwrap_or(usize::MAX));
    Ok(buf)
}

pub(crate) fn read_range(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let total = positioned::read_at_most(file, offset, &mut buf).map_err(Error::Io)?;
    buf.truncate(total);
    Ok(buf)
}

/// Positioned IO that does not race on a shared file cursor.
#[cfg(unix)]
mod positioned {
    use std::fs::File;
    use std::os::unix::fs::FileExt;

    /// `pwrite(2)` loop via `FileExt::write_all_at`. Leaves the file
    /// cursor untouched; `EINTR` is retried and a zero-byte write is an
    /// `ErrorKind::WriteZero` error.
    pub(super) fn write_all_at(file: &File, offset: u64, data: &[u8]) -> std::io::Result<()> {
        file.write_all_at(data, offset)
    }

    /// `pread(2)` loop that fills `buf` or stops at end of file. Returns
    /// the number of bytes read.
    pub(super) fn read_at_most(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut total = 0usize;
        while total < buf.len() {
            let pos = offset.checked_add(total as u64).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "read_range: offset overflow",
                )
            })?;
            match file.read_at(&mut buf[total..], pos) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }
}

/// Positioned IO that does not race on a shared file cursor.
///
/// Without `pread` / `pwrite` the only portable primitive is
/// seek-then-IO. Every positioned call in the process takes this lock so
/// two calls cannot interleave their seeks, and each call restores the
/// cursor it found.
#[cfg(not(unix))]
mod positioned {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::Mutex;

    static POSITIONED_IO: Mutex<()> = Mutex::new(());

    fn with_cursor_at<T>(
        file: &File,
        offset: u64,
        op: impl FnOnce(&mut &File) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        // A poisoned lock only means another thread panicked mid-IO; the
        // `()` payload carries no state to repair.
        let _guard = POSITIONED_IO
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut handle = file;
        let saved = handle.stream_position()?;
        let _new_pos = handle.seek(SeekFrom::Start(offset))?;
        let result = op(&mut handle);
        let restored = handle.seek(SeekFrom::Start(saved));
        let value = result?;
        let _restored_pos = restored?;
        Ok(value)
    }

    pub(super) fn write_all_at(file: &File, offset: u64, data: &[u8]) -> std::io::Result<()> {
        with_cursor_at(file, offset, |f| f.write_all(data))
    }

    pub(super) fn read_at_most(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        with_cursor_at(file, offset, |f| {
            let mut total = 0usize;
            while total < buf.len() {
                match f.read(&mut buf[total..]) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(total)
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Durability
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn sync_data(file: &File) -> Result<()> {
    file.sync_all().map_err(Error::Io)
}

pub(crate) fn sync_full(file: &File) -> Result<()> {
    file.sync_all().map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Rename, directory sync, copy
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(Error::Io)
}

pub(crate) fn sync_parent_dir(_path: &Path) -> Result<()> {
    // No portable way to sync a directory; no-op on unknown platforms.
    Ok(())
}

pub(crate) fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    std::fs::copy(src, dst).map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Probes
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn probe_sector_size(_path: &Path) -> u32 {
    // Unknown platform; return the safe default.
    512
}

// Storage-engine primitives — no-op on unknown platforms.
pub(crate) fn preallocate(_file: &File, _offset: u64, _len: u64) -> Result<()> {
    Ok(())
}

pub(crate) fn advise(_file: &File, _offset: u64, _len: u64, _advice: crate::Advice) -> Result<()> {
    Ok(())
}

/// 0.9.6 — Hole punching is platform-specific (Linux `fallocate`, macOS
/// `F_PUNCHHOLE`, Windows `FSCTL_SET_ZERO_DATA`) and there's no portable
/// fallback that preserves the contract ("storage reclaimed, length
/// unchanged, reads return zeros"). A buffered zero-fill would satisfy
/// the read-back-zeros half but not the storage-reclamation half, so
/// surfacing `ErrorKind::Unsupported` is the honest answer for the
/// fallback platform.
pub(crate) fn punch_hole(_file: &File, _offset: u64, _len: u64) -> Result<()> {
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "punch_hole not supported on this platform",
    )))
}

pub(crate) fn probe_direct_io_available() -> bool {
    false
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("fsys_unk_{}_{}_{}", std::process::id(), n, suffix))
    }

    struct TmpFile(std::path::PathBuf);
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_write_read_roundtrip() {
        let path = tmp_path("rw");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"unknown platform").expect("write");
        drop(f);
        let (rf, _) = open_read(&path, false).expect("read");
        let data = read_all(&rf).expect("read_all");
        assert_eq!(data, b"unknown platform");
    }

    #[test]
    fn test_probe_sector_size_returns_512() {
        assert_eq!(probe_sector_size(Path::new(".")), 512);
    }

    #[test]
    fn test_probe_direct_io_available_is_false() {
        assert!(!probe_direct_io_available());
    }

    #[test]
    fn test_write_at_does_not_move_cursor_and_lands_at_offset() {
        let path = tmp_path("pos");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"0123456789").expect("seed");
        let f = open_write_at(&path).expect("open");
        write_at(&f, 4, b"ab").expect("write_at");
        write_all(&f, b"Z").expect("cursor write");
        drop(f);
        // The cursor write lands at 0 because write_at left it there.
        assert_eq!(std::fs::read(&path).expect("read"), b"Z123ab6789");
    }

    #[test]
    fn test_concurrent_write_at_lands_every_record() {
        let path = tmp_path("concurrent");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"").expect("seed");
        let f = std::sync::Arc::new(open_write_at(&path).expect("open"));
        let threads: Vec<_> = (0u8..8)
            .map(|t| {
                let f = std::sync::Arc::clone(&f);
                std::thread::spawn(move || {
                    for i in 0u64..64 {
                        let slot = u64::from(t) * 64 + i;
                        write_at(&f, slot * 4, &[t; 4]).expect("write_at");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("join");
        }
        drop(f);
        let data = std::fs::read(&path).expect("read");
        assert_eq!(data.len(), 8 * 64 * 4);
        for (slot, chunk) in data.chunks(4).enumerate() {
            let t = (slot / 64) as u8;
            assert_eq!(chunk, &[t; 4], "slot {slot}");
        }
    }

    #[test]
    fn test_read_range_short_at_eof_and_empty_past_eof() {
        let path = tmp_path("range");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"hello").expect("seed");
        let (f, _) = open_read(&path, false).expect("open");
        assert_eq!(read_range(&f, 1, 3).expect("mid"), b"ell");
        assert_eq!(read_range(&f, 3, 10).expect("tail"), b"lo");
        assert!(read_range(&f, 10, 4).expect("past eof").is_empty());
    }

    #[test]
    fn test_atomic_rename() {
        let src = tmp_path("ren_src");
        let dst = tmp_path("ren_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"content").expect("write");
        atomic_rename(&src, &dst).expect("rename");
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).expect("read"), b"content");
    }
}
