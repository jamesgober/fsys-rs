//! File CRUD operations implemented as `impl Handle`.
//!
//! All writes use an atomic temp-rename pattern:
//! 1. Write to a temp file (`.fsys-tmp-<n>.<target>`).
//! 2. Flush to the requested durability level.
//! 3. `rename(temp, target)` — atomic on POSIX; `MoveFileExW` on Windows.
//! 4. Sync the parent directory (Linux/macOS; no-op on Windows).
//!
//! On any failure after temp creation, the temp file is removed with a
//! best-effort delete (failure to clean up is not reported back to the
//! caller because the primary error has already been set).

use crate::handle::Handle;
use crate::meta::FileMeta;
use crate::method::Method;
use crate::platform;
use crate::{Error, Result};
use std::path::Path;

impl Handle {
    // ──────────────────────────────────────────────────────────────────────────
    // Writing
    // ──────────────────────────────────────────────────────────────────────────

    /// Atomically writes `data` to `path`, replacing any existing file.
    ///
    /// The write is flushed at the durability level configured on this
    /// handle before the atomic rename. If the active method falls back to
    /// a less-capable strategy (e.g. `O_DIRECT` rejected), the handle's
    /// active method is updated automatically.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::AtomicReplaceFailed`] if any step in the atomic sequence
    ///   fails.
    pub fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        let temp = Self::gen_temp_path(&path);

        // Step 1: open the temp file.
        let (file, direct_ok) =
            platform::open_write_new(&temp, self.use_direct()).map_err(|e| {
                Error::AtomicReplaceFailed {
                    step: "open_temp",
                    source: as_io_error(e),
                }
            })?;

        if self.use_direct() && !direct_ok {
            self.update_active_method(Method::Data);
        }

        // Step 2: write data.
        let write_result = if direct_ok {
            platform::write_all_direct(&file, data, self.sector_size())
        } else {
            platform::write_all(&file, data)
        };
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::AtomicReplaceFailed {
                step: "write",
                source: as_io_error(e),
            });
        }

        if direct_ok {
            // Step 3 (Direct IO path): FILE_FLAG_NO_BUFFERING writes are
            // sector-padded. Drop the NO_BUFFERING handle (WRITE_THROUGH
            // already ensures bytes are on disk), then reopen buffered solely
            // to truncate the file back to the actual data length.
            drop(file);
            if let Err(e) = std::fs::OpenOptions::new()
                .write(true)
                .open(&temp)
                .and_then(|f| f.set_len(data.len() as u64))
            {
                let _ = std::fs::remove_file(&temp);
                return Err(Error::AtomicReplaceFailed {
                    step: "truncate",
                    source: e,
                });
            }
        } else {
            // Step 3 (Buffered path): explicit flush for durability.
            let flush_result = self.flush_file(&file, false);
            if let Err(e) = flush_result {
                let _ = std::fs::remove_file(&temp);
                return Err(Error::AtomicReplaceFailed {
                    step: "flush",
                    source: as_io_error(e),
                });
            }

            // Step 4: close before rename.
            drop(file);
        }

        // Step 5: atomic rename.
        if let Err(e) = platform::atomic_rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::AtomicReplaceFailed {
                step: "rename",
                source: as_io_error(e),
            });
        }

        // Step 6: sync parent dir (no-op on Windows; best-effort on others).
        let _ = platform::sync_parent_dir(&path);

        Ok(())
    }

    /// Appends `data` to `path`, creating the file if it does not exist.
    ///
    /// Append does not use the atomic-rename pattern because the caller
    /// explicitly wants to extend an existing file. The write is NOT
    /// individually flushed; call [`Handle::sync`] if you need durability
    /// after a batch of appends.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn append(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        let mut file = platform::open_append(&path)?;
        use std::io::Write;
        file.write_all(data).map_err(Error::Io)
    }

    /// Writes `data` at byte `offset` in `path`.
    ///
    /// Direct IO is NOT used for positioned writes because arbitrary offsets
    /// require a read-modify-write cycle at the sector boundary, which
    /// removes the performance benefit of Direct IO. The write is buffered
    /// and uses the standard sync primitive for this handle's method.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn write_at(&self, path: impl AsRef<Path>, offset: u64, data: &[u8]) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        let file = platform::open_write_at(&path)?;
        platform::write_at(&file, offset, data)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Reading
    // ──────────────────────────────────────────────────────────────────────────

    /// Reads the entire contents of `path`.
    ///
    /// Uses Direct IO when the handle's active method is `Direct`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        let path = self.resolve_path(path.as_ref())?;
        let (file, direct_ok) = platform::open_read(&path, self.use_direct())?;

        if self.use_direct() && !direct_ok {
            self.update_active_method(Method::Data);
        }

        if direct_ok {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            platform::read_all_direct(&file, size, self.sector_size())
        } else {
            platform::read_all(&file)
        }
    }

    /// Reads `len` bytes from `path` starting at byte `offset`.
    ///
    /// If fewer than `len` bytes are available (EOF), the returned `Vec`
    /// will be shorter than `len`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn read_range(&self, path: impl AsRef<Path>, offset: u64, len: usize) -> Result<Vec<u8>> {
        let path = self.resolve_path(path.as_ref())?;
        let (file, _) = platform::open_read(&path, false)?;
        platform::read_range(&file, offset, len)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Deletion and existence
    // ──────────────────────────────────────────────────────────────────────────

    /// Deletes the file at `path`.
    ///
    /// This operation is **idempotent**: if the file does not exist,
    /// `Ok(())` is returned.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] for any error other than "not found".
    pub fn delete(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Returns `true` if `path` exists and is a regular file.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on any IO error other than "not found".
    pub fn exists(&self, path: impl AsRef<Path>) -> Result<bool> {
        let path = self.resolve_path(path.as_ref())?;
        match std::fs::metadata(&path) {
            Ok(m) => Ok(m.is_file()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Returns the size of the file at `path` in bytes.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] if the file does not exist or is not readable.
    pub fn size(&self, path: impl AsRef<Path>) -> Result<u64> {
        let path = self.resolve_path(path.as_ref())?;
        std::fs::metadata(&path).map(|m| m.len()).map_err(Error::Io)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Copy and metadata
    // ──────────────────────────────────────────────────────────────────────────

    /// Copies `src` to `dst` using a platform-optimised copy primitive.
    ///
    /// On Linux, `copy_file_range(2)` will be used in a future release
    /// (0.5.0); currently falls back to `std::fs::copy`. On macOS, a
    /// `clonefile(2)` reflink optimisation is planned for 0.5.0.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if either path escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn copy(&self, src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<u64> {
        let src = self.resolve_path(src.as_ref())?;
        let dst = self.resolve_path(dst.as_ref())?;
        platform::copy_file(&src, &dst)
    }

    /// Returns [`FileMeta`] for the file at `path`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] if the file does not exist or is not accessible.
    pub fn meta(&self, path: impl AsRef<Path>) -> Result<FileMeta> {
        let path = self.resolve_path(path.as_ref())?;
        let m = std::fs::metadata(&path).map_err(Error::Io)?;
        Ok(FileMeta::from_metadata(&m, &path))
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Durability helpers
    // ──────────────────────────────────────────────────────────────────────────

    /// Flushes the OS write buffers for an open file.
    ///
    /// The exact primitive depends on the handle's active method:
    /// - `Sync`: full fsync (data + metadata).
    /// - `Data`: fdatasync on Linux, full sync elsewhere.
    /// - `Direct`: no separate flush (data is already on media).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the flush syscall fails.
    pub fn sync(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        let (file, _) = platform::open_write_at(&path).map(|f| (f, false))?;
        self.flush_file(&file, false)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Internal
    // ──────────────────────────────────────────────────────────────────────────

    fn flush_file(&self, file: &std::fs::File, is_direct: bool) -> Result<()> {
        match self.active_method() {
            Method::Direct if is_direct => {
                // On Windows, FILE_FLAG_WRITE_THROUGH already flushed on each
                // write. On Linux/macOS, issue fdatasync / F_FULLFSYNC.
                #[cfg(not(target_os = "windows"))]
                {
                    platform::sync_data(file)
                }
                #[cfg(target_os = "windows")]
                {
                    Ok(())
                }
            }
            Method::Data => platform::sync_data(file),
            _ => platform::sync_full(file),
        }
    }
}

// Convert a `crate::Error` to a `std::io::Error` for use in
// `AtomicReplaceFailed { source: std::io::Error }`.
fn as_io_error(e: Error) -> std::io::Error {
    match e {
        Error::Io(io_err) => io_err,
        other => std::io::Error::other(other.to_string()),
    }
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::builder::Builder;
    use crate::method::Method;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_crud_file_{}_{}_{}",
            std::process::id(),
            n,
            suffix
        ))
    }

    struct TmpFile(std::path::PathBuf);
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn handle() -> crate::handle::Handle {
        Builder::new()
            .method(Method::Sync)
            .build()
            .expect("build handle")
    }

    #[test]
    fn test_write_creates_file() {
        let path = tmp_path("write");
        let _g = TmpFile(path.clone());
        let h = handle();
        h.write(&path, b"hello").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"hello");
    }

    #[test]
    fn test_write_replaces_existing() {
        let path = tmp_path("replace");
        let _g = TmpFile(path.clone());
        let h = handle();
        h.write(&path, b"old").expect("write old");
        h.write(&path, b"new").expect("write new");
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
    }

    #[test]
    fn test_read_roundtrip() {
        let path = tmp_path("read");
        let _g = TmpFile(path.clone());
        let h = handle();
        h.write(&path, b"read test").expect("write");
        let data = h.read(&path).expect("read");
        assert_eq!(data, b"read test");
    }

    #[test]
    fn test_append_accumulates() {
        let path = tmp_path("append");
        let _g = TmpFile(path.clone());
        let h = handle();
        h.append(&path, b"line1\n").expect("append 1");
        h.append(&path, b"line2\n").expect("append 2");
        let data = std::fs::read(&path).expect("read");
        assert_eq!(data, b"line1\nline2\n");
    }

    #[test]
    fn test_delete_idempotent() {
        let path = tmp_path("delete");
        let h = handle();
        // File does not exist — should be Ok.
        h.delete(&path).expect("delete non-existent");
        // Create then delete.
        h.write(&path, b"x").expect("write");
        h.delete(&path).expect("delete existing");
        assert!(!path.exists());
        // Delete again — still Ok.
        h.delete(&path).expect("delete already deleted");
    }

    #[test]
    fn test_exists_reflects_state() {
        let path = tmp_path("exists");
        let _g = TmpFile(path.clone());
        let h = handle();
        assert!(!h.exists(&path).expect("exists before create"));
        h.write(&path, b"x").expect("write");
        assert!(h.exists(&path).expect("exists after create"));
    }

    #[test]
    fn test_size_returns_correct_bytes() {
        let path = tmp_path("size");
        let _g = TmpFile(path.clone());
        let h = handle();
        h.write(&path, b"12345").expect("write");
        assert_eq!(h.size(&path).expect("size"), 5);
    }

    #[test]
    fn test_copy_produces_identical_content() {
        let src = tmp_path("cp_src");
        let dst = tmp_path("cp_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        let h = handle();
        h.write(&src, b"copy content").expect("write src");
        let _bytes = h.copy(&src, &dst).expect("copy");
        assert_eq!(std::fs::read(&dst).expect("read dst"), b"copy content");
    }

    #[test]
    fn test_read_range_returns_slice() {
        let path = tmp_path("range");
        let _g = TmpFile(path.clone());
        let h = handle();
        h.write(&path, b"0123456789").expect("write");
        let chunk = h.read_range(&path, 3, 4).expect("read_range");
        assert_eq!(chunk, b"3456");
    }
}
