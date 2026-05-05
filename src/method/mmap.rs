//! `Method::Mmap` backend — memory-mapped IO with explicit `msync` /
//! `FlushViewOfFile` for durability.
//!
//! Real implementation lands in `0.5.0`. Reads come from a mapped
//! region; writes flow through a mapping over the atomic-replace temp
//! file. The mapping's lifetime is bounded by the operation — no
//! mapping outlives the call (locked decision #4 in
//! `.dev/DECISIONS-0.5.0.md`).
//!
//! ## Crash safety
//!
//! Writes use the same atomic-replace pattern as
//! [`crate::Method::Sync`]: all bytes go to a temp file first, then
//! `msync(MS_SYNC)` (Linux/macOS) or `FlushViewOfFile` (Windows)
//! flushes the mapping, then `atomic_rename` makes the new contents
//! visible at the target path. The contract is identical to the Sync
//! path: the file is either entirely-old or entirely-new — never
//! torn — at every observable point.
//!
//! ## Fallback to `Method::Sync`
//!
//! Per locked decision #4 + R-2'' in `.dev/DECISIONS-0.5.0.md`, the
//! mmap path falls back permanently to Sync (per-Handle, observable
//! via [`crate::Handle::active_method`]) when:
//!
//! - The payload is smaller than the system page size (overhead
//!   exceeds benefit).
//! - The target/source is not a regular file.
//! - The kernel rejects the `mmap` syscall.

use std::path::Path;

use memmap2::{Mmap, MmapMut};

use crate::handle::Handle;
use crate::{Error, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Suitability checks
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `true` when the mmap write path is appropriate for this
/// payload size.
///
/// Per R-2'' the mmap path is unsuitable for sub-page payloads and
/// zero-length writes. The caller is expected to fall back to the Sync
/// atomic-replace path when this returns `false`.
pub(crate) fn is_suitable_for_write(data_len: usize) -> bool {
    if data_len == 0 {
        return false;
    }
    let page_size = crate::os::info().page_size;
    data_len >= page_size
}

/// Returns `true` when the mmap read path is appropriate for this file.
///
/// Per R-2'' the mmap path is unsuitable for sub-page files, zero-byte
/// files, and non-regular files (sockets, pipes, FIFOs, special
/// character devices).
pub(crate) fn is_suitable_for_read(meta: &std::fs::Metadata) -> bool {
    if !meta.is_file() {
        return false;
    }
    let len = meta.len();
    if len == 0 {
        return false;
    }
    let page_size = crate::os::info().page_size as u64;
    len >= page_size
}

// ─────────────────────────────────────────────────────────────────────────────
// Write path — atomic-replace via mmap-populated temp file
// ─────────────────────────────────────────────────────────────────────────────

/// Atomic-replace write through a temporary memory mapping.
///
/// Preconditions: the caller has already checked
/// [`is_suitable_for_write`]. Calling this with a sub-page or
/// zero-length payload is a programmer error and produces
/// [`Error::MmapFailed`].
///
/// # Errors
///
/// - [`Error::MmapFailed`] when the mapping cannot be created or the
///   `msync` / `FlushViewOfFile` flush fails.
/// - [`Error::AtomicReplaceFailed`] when temp creation, write, or
///   rename fails (mirrors the Sync path's error variants for
///   uniformity).
pub(crate) fn write(path: &Path, data: &[u8]) -> Result<()> {
    if !is_suitable_for_write(data.len()) {
        return Err(Error::MmapFailed {
            reason: format!(
                "payload of {} bytes is below page size; caller should fall back to Sync",
                data.len()
            ),
        });
    }

    let temp = Handle::gen_temp_path(path);

    // Step 1 — create + size the temp file.
    let temp_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| Error::AtomicReplaceFailed {
            step: "open_temp",
            source: e,
        })?;
    if let Err(e) = temp_file.set_len(data.len() as u64) {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::AtomicReplaceFailed {
            step: "set_len",
            source: e,
        });
    }

    // Step 2 — create the mmap and copy bytes through it.
    // SAFETY: `temp` was just created via `create_new(true)` — the path
    // is exclusively owned by this operation. No other process holds
    // a handle to it; modifications via the mapping are private until
    // `atomic_rename` reveals the file at `path`. The mapping does not
    // outlive `temp_file` (both drop at end of scope).
    let mut mmap = match unsafe { MmapMut::map_mut(&temp_file) } {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::MmapFailed {
                reason: format!("MmapMut::map_mut failed for temp: {e}"),
            });
        }
    };
    mmap.copy_from_slice(data);

    // Step 3 — flush the mapping (msync / FlushViewOfFile). This
    // flushes dirty data pages to stable storage but on Linux/macOS
    // does NOT include a metadata sync.
    if let Err(e) = mmap.flush() {
        drop(mmap);
        let _ = std::fs::remove_file(&temp);
        return Err(Error::MmapFailed {
            reason: format!("Mmap::flush (msync/FlushViewOfFile) failed: {e}"),
        });
    }

    // Step 3b — fsync the file to sync inode metadata (file size).
    // POSIX `msync(MS_SYNC)` covers data pages but not metadata; an
    // atomic-rename of a file whose data is durable but whose size
    // metadata is stale would surface as a 0-length file after a
    // power-loss event. `fsync` after `msync` closes that window.
    // On Windows `FlushViewOfFile` covers data; `FlushFileBuffers`
    // covers metadata — both happen via the same call here.
    if let Err(e) = temp_file.sync_all() {
        drop(mmap);
        let _ = std::fs::remove_file(&temp);
        return Err(Error::MmapFailed {
            reason: format!("fsync of mmap temp file failed: {e}"),
        });
    }

    // Step 4 — drop the mapping and the temp file before rename.
    drop(mmap);
    drop(temp_file);

    // Step 5 — atomic rename.
    if let Err(e) = crate::platform::atomic_rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::AtomicReplaceFailed {
            step: "rename",
            source: as_io_error(e),
        });
    }

    // Step 6 — best-effort parent-dir sync (no-op on Windows).
    let _ = crate::platform::sync_parent_dir(path);

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Read path — open + map + copy + drop
// ─────────────────────────────────────────────────────────────────────────────

/// Reads the entire file at `path` through a read-only memory mapping.
///
/// Preconditions: the caller has already checked
/// [`is_suitable_for_read`] using the file's metadata. Calling this on
/// an unsuitable file (sub-page, zero-byte, non-regular) produces
/// [`Error::MmapFailed`].
///
/// # Errors
///
/// - [`Error::Io`] when the file cannot be opened.
/// - [`Error::MmapFailed`] when the mapping cannot be created.
pub(crate) fn read(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).map_err(Error::Io)?;
    let meta = file.metadata().map_err(Error::Io)?;
    if !is_suitable_for_read(&meta) {
        return Err(Error::MmapFailed {
            reason: format!(
                "file is unsuitable for mmap (size={}, is_file={}); caller should fall back to Sync",
                meta.len(),
                meta.is_file()
            ),
        });
    }

    // SAFETY: a `MAP_PRIVATE` mapping (the default for
    // `memmap2::Mmap::map`) gives us a copy-on-write view: concurrent
    // modifications by other processes via the page cache do not
    // corrupt our snapshot. We never write through this mapping. The
    // mapping does not outlive `file` (both drop at end of scope).
    let mmap = match unsafe { Mmap::map(&file) } {
        Ok(m) => m,
        Err(e) => {
            return Err(Error::MmapFailed {
                reason: format!("Mmap::map failed: {e}"),
            });
        }
    };

    let owned = mmap.to_vec();
    drop(mmap);
    drop(file);
    Ok(owned)
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Mirrors `crud::file::as_io_error` so this module is self-contained.
fn as_io_error(e: Error) -> std::io::Error {
    match e {
        Error::Io(io_err) => io_err,
        other => std::io::Error::other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static C: AtomicU64 = AtomicU64::new(0);

    fn tmp(suffix: &str) -> std::path::PathBuf {
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("fsys_mmap_{}_{}_{}", std::process::id(), n, suffix))
    }

    struct TmpFile(std::path::PathBuf);
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn payload_at_least_page_size() -> Vec<u8> {
        let n = crate::os::info().page_size;
        let mut v = vec![0u8; n.max(4096)];
        for (i, b) in v.iter_mut().enumerate() {
            *b = (i % 251) as u8; // arbitrary deterministic pattern
        }
        v
    }

    // ── Suitability checks ─────────────────────────────────────────

    #[test]
    fn test_is_suitable_for_write_rejects_zero_len() {
        assert!(!is_suitable_for_write(0));
    }

    #[test]
    fn test_is_suitable_for_write_rejects_sub_page() {
        assert!(!is_suitable_for_write(128));
    }

    #[test]
    fn test_is_suitable_for_write_accepts_page_or_larger() {
        let page = crate::os::info().page_size;
        assert!(is_suitable_for_write(page));
        assert!(is_suitable_for_write(page * 4));
    }

    // ── Write path ─────────────────────────────────────────────────

    #[test]
    fn test_write_atomic_replace_round_trip() {
        let path = tmp("write_round");
        let _g = TmpFile(path.clone());
        let data = payload_at_least_page_size();
        write(&path, &data).expect("mmap write");
        let actual = std::fs::read(&path).expect("read");
        assert_eq!(actual, data);
    }

    #[test]
    fn test_write_replaces_existing_file() {
        let path = tmp("write_replace");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"old short content").unwrap();
        let new_data = payload_at_least_page_size();
        write(&path, &new_data).expect("mmap write");
        let actual = std::fs::read(&path).expect("read");
        assert_eq!(actual, new_data);
    }

    #[test]
    fn test_write_rejects_zero_length_payload() {
        let path = tmp("write_zero");
        let _g = TmpFile(path.clone());
        let result = write(&path, &[]);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::MmapFailed { reason } => assert!(reason.contains("0 bytes")),
            other => panic!("expected MmapFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_write_rejects_sub_page_payload() {
        let path = tmp("write_subpage");
        let _g = TmpFile(path.clone());
        let result = write(&path, b"only a few bytes");
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::MmapFailed { reason } => {
                assert!(reason.contains("below page size"))
            }
            other => panic!("expected MmapFailed, got {:?}", other),
        }
    }

    // ── Read path ──────────────────────────────────────────────────

    #[test]
    fn test_read_returns_full_contents() {
        let path = tmp("read_round");
        let _g = TmpFile(path.clone());
        let data = payload_at_least_page_size();
        std::fs::write(&path, &data).unwrap();
        let actual = read(&path).expect("mmap read");
        assert_eq!(actual, data);
    }

    #[test]
    fn test_read_rejects_zero_byte_file() {
        let path = tmp("read_zero");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"").unwrap();
        let result = read(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::MmapFailed { .. } => { /* expected */ }
            other => panic!("expected MmapFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_read_rejects_sub_page_file() {
        let path = tmp("read_subpage");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, b"tiny").unwrap();
        let result = read(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_read_returns_io_error_for_missing_file() {
        let path = tmp("read_missing");
        let result = read(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Error::Io NotFound, got {:?}", other),
        }
    }
}
