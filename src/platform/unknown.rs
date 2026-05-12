//! Fallback IO primitives for unknown/unsupported platforms.
//!
//! Uses only `std::fs` and `std::io` — no Direct IO, no platform syscalls.
//! `probe_direct_io_available()` returns `false` so the method resolver
//! always selects `Method::Sync` on these targets.

#![cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]

use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
    OpenOptions::new()
        .write(true)
        .create(true)
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
    let mut f = file.try_clone().map_err(Error::Io)?;
    f.seek(SeekFrom::Start(offset)).map_err(Error::Io)?;
    f.write_all(data).map_err(Error::Io)
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
    (&*file).read_to_end(&mut buf).map_err(Error::Io)?;
    Ok(buf)
}

pub(crate) fn read_all_direct(file: &File, file_size: u64, _sector_size: u32) -> Result<Vec<u8>> {
    // No Direct IO; read normally and trim to file_size.
    let mut buf = Vec::new();
    (&*file).read_to_end(&mut buf).map_err(Error::Io)?;
    let trimmed = usize::min(buf.len(), file_size as usize);
    buf.truncate(trimmed);
    Ok(buf)
}

pub(crate) fn read_range(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut f = file.try_clone().map_err(Error::Io)?;
    f.seek(SeekFrom::Start(offset)).map_err(Error::Io)?;
    let mut buf = vec![0u8; len];
    let mut total = 0usize;
    while total < len {
        let n = f.read(&mut buf[total..]).map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    buf.truncate(total);
    Ok(buf)
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
