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
    /// The write follows the temp-file + atomic-rename pattern: a new
    /// file is created at `<path>.fsys-tmp-<n>`, `data` is written and
    /// flushed at the handle's durability level, then a single
    /// `rename(2)` / `MoveFileExW` swaps it into place. After this
    /// method returns successfully, the target file is durably on
    /// stable storage and a concurrent reader sees either the entire
    /// new payload or the previous file content — never a partial
    /// write.
    ///
    /// If the active method falls back to a less-capable strategy
    /// (e.g. `O_DIRECT` rejected by tmpfs, `Mmap` rejected by sub-page
    /// payload), the handle's active method is updated automatically
    /// and observable via [`Handle::active_method`].
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::AtomicReplaceFailed`] if any step in the atomic
    ///   sequence (open / write / flush / rename / sync_parent) fails.
    ///   The error's `step` field identifies which step failed; the
    ///   original target file is unmodified for any failure before
    ///   `rename`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::builder;
    ///
    /// # fn example() -> fsys::Result<()> {
    /// let fs = builder().build()?;
    /// fs.write("/etc/myapp/config.toml", b"key = \"value\"\n")?;
    /// // Target file is now durably "key = \"value\"\n", or still
    /// // the old content (if a crash happened mid-call) — never
    /// // a torn mix.
    /// # Ok(())
    /// # }
    /// ```
    pub fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        #[cfg(feature = "tracing")]
        let _span = tracing::trace_span!(
            "fsys::Handle::write",
            payload_bytes = data.len(),
            method = self.active_method().as_str(),
        )
        .entered();

        let path = self.resolve_path(path.as_ref())?;

        // 0.5.0: route Method::Mmap through the mmap atomic-replace
        // path when the payload is suitable. Sub-page payloads (and
        // zero-length writes) fall back permanently to Sync per
        // R-2'' in `.dev/DECISIONS-0.5.0.md`. The fallback updates
        // the Handle's active_method so subsequent ops on this
        // Handle take the Sync path directly without the
        // suitability check.
        if self.active_method() == Method::Mmap {
            if crate::method::mmap::is_suitable_for_write(data.len()) {
                return crate::method::mmap::write(&path, data);
            }
            self.update_active_method(Method::Sync);
        }

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
            self.direct_write(&file, &path, data)
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
            // Step 3 (Direct IO path): FILE_FLAG_NO_BUFFERING /
            // O_DIRECT writes are sector-padded. Truncate to the
            // actual data length on the SAME open file handle —
            // `set_len` on the original `file` calls `ftruncate`
            // (Unix) or `SetFilePointerEx + SetEndOfFile` (Windows).
            // Both work regardless of the open flags. Earlier
            // versions dropped the file and reopened buffered just
            // to call `set_len` — that wasted two syscalls
            // (close + open) per Direct write. (0.8.0 I-checkpoint
            // perf fix.)
            if let Err(e) = file.set_len(data.len() as u64) {
                drop(file);
                let _ = std::fs::remove_file(&temp);
                return Err(Error::AtomicReplaceFailed {
                    step: "truncate",
                    source: e,
                });
            }
            drop(file);
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

    /// Atomically replaces `path` with `data`, preserving the target's
    /// existing metadata.
    ///
    /// **What "copy" means here:** this is *not* a file-to-file copy
    /// operation (no source path argument). It is a write that
    /// **copies the existing target's metadata onto the new payload
    /// before swapping it in** — mode, ACLs, ownership, timestamps. If
    /// you need a real file-to-file copy, use
    /// [`std::fs::copy`]; fsys does not provide one.
    ///
    /// Implemented as **atomic swap only** — the file at `path` is
    /// either entirely-old or entirely-new at every observable point.
    /// No partial replace, no in-place mutation. Compared to
    /// [`Handle::write`], `write_copy` preserves the target file's
    /// existing metadata where the OS supports it and the calling
    /// process has permission:
    ///
    /// - **Unix:** mode is preserved unconditionally; owner/group is
    ///   preserved only when the process has `CAP_CHOWN` or
    ///   equivalent (silently skipped otherwise).
    /// - **Windows:** ACLs are preserved via `GetSecurityInfo` /
    ///   `SetSecurityInfo`.
    /// - **All platforms:** `mtime` and `atime` are preserved.
    ///
    /// If `path` does not exist, `write_copy` behaves identically to
    /// [`Handle::write`] (creates a new file with default
    /// permissions).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::AtomicReplaceFailed`] if any step in the atomic
    ///   sequence (open temp, write, flush, rename, sync parent)
    ///   fails. Metadata-preservation failures that surface as
    ///   `EPERM` (e.g. trying to `chown` without `CAP_CHOWN`) are
    ///   silently skipped, not surfaced.
    ///
    /// # Crash safety
    ///
    /// Same contract as [`Handle::write`]: the target file is either
    /// entirely the old payload (kill before rename) or entirely the
    /// new payload (kill after rename). Never torn. The metadata-
    /// preservation step happens on the staging file before the
    /// rename, so a crash mid-`chmod` leaves the target file
    /// unchanged.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::builder;
    ///
    /// let fs = builder().build()?;
    /// // Replace `/etc/foo.conf` keeping its 0644 mode bits intact:
    /// fs.write_copy("/etc/foo.conf", b"new contents")?;
    /// # Ok::<(), fsys::Error>(())
    /// ```
    pub fn write_copy(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;

        // 1. Capture existing metadata if the target exists. This
        //    is best-effort — if the metadata read fails we proceed
        //    with default permissions (the same as `write`).
        let existing_meta = std::fs::metadata(&path).ok();

        // 2. Build the staging file via the same atomic-replace
        //    primitives as `write`. We do not call `self.write`
        //    directly because we need access to the staging path
        //    before the rename in order to apply metadata.
        let temp = Self::gen_temp_path(&path);
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

        let write_result = if direct_ok {
            self.direct_write(&file, &path, data)
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
            let flush_result = self.flush_file(&file, false);
            if let Err(e) = flush_result {
                let _ = std::fs::remove_file(&temp);
                return Err(Error::AtomicReplaceFailed {
                    step: "flush",
                    source: as_io_error(e),
                });
            }
            drop(file);
        }

        // 3. Apply preserved metadata to the staging file BEFORE
        //    the rename, so that the rename is the single
        //    observable transition.
        if let Some(meta) = existing_meta.as_ref() {
            apply_preserved_metadata(&temp, &path, meta);
        }

        // 4. Atomic rename.
        if let Err(e) = platform::atomic_rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::AtomicReplaceFailed {
                step: "rename",
                source: as_io_error(e),
            });
        }

        let _ = platform::sync_parent_dir(&path);
        Ok(())
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

        // 0.5.0: Method::Mmap reads consult metadata first to check
        // suitability. Files smaller than the page size, zero-byte
        // files, and non-regular files (sockets/pipes/FIFOs) cause a
        // permanent fallback to Sync per R-2'' in
        // `.dev/DECISIONS-0.5.0.md`.
        if self.active_method() == Method::Mmap {
            let suitable = std::fs::metadata(&path)
                .map(|m| crate::method::mmap::is_suitable_for_read(&m))
                .unwrap_or(false);
            if suitable {
                return crate::method::mmap::read(&path);
            }
            self.update_active_method(Method::Sync);
        }

        let (file, direct_ok) = platform::open_read(&path, self.use_direct())?;

        if self.use_direct() && !direct_ok {
            self.update_active_method(Method::Data);
        }

        if direct_ok {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            self.direct_read(&file, size)
        } else {
            platform::read_all(&file)
        }
    }

    /// Reads `len` bytes from `path` starting at byte `offset`.
    ///
    /// If fewer than `len` bytes are available (EOF), the returned `Vec`
    /// will be shorter than `len`.
    ///
    /// Symmetric with [`Handle::write_at`] — both target a positioned
    /// IO operation. Renamed from `read_range` in `0.7.0` per the
    /// API audit (see `.dev/API-AUDIT-0.7.0.md` H.1 Open Question 2)
    /// to match POSIX `pread` / `pwrite` symmetry.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn read_at(&self, path: impl AsRef<Path>, offset: u64, len: usize) -> Result<Vec<u8>> {
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
    /// Currently routes through `std::fs::copy`. Linux
    /// `copy_file_range(2)` and macOS `clonefile(2)` reflink
    /// optimisations are filed for a future release (deferred from
    /// the originally-planned `0.5.0` slot; not part of the `0.6.0`
    /// scope).
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

    /// Resizes the file at `path` to `new_size` bytes.
    ///
    /// Truncating to a smaller size discards the trailing bytes.
    /// Extending to a larger size pads with zero bytes (sparse on
    /// filesystems that support sparse files; physical zero on
    /// those that don't).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn truncate(&self, path: impl AsRef<Path>, new_size: u64) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(Error::Io)?;
        f.set_len(new_size).map_err(Error::Io)
    }

    /// Renames or moves a file from `old` to `new`.
    ///
    /// On Linux/macOS this is a `rename(2)` call — atomic within a
    /// single filesystem. On Windows it uses `MoveFileExW` with
    /// `MOVEFILE_REPLACE_EXISTING`. Cross-filesystem renames may
    /// fall back to copy-and-delete (platform-dependent); within a
    /// single filesystem the operation is atomic.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if either path escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn rename(&self, old: impl AsRef<Path>, new: impl AsRef<Path>) -> Result<()> {
        let old = self.resolve_path(old.as_ref())?;
        let new = self.resolve_path(new.as_ref())?;
        platform::atomic_rename(&old, &new)
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

    /// Direct-IO write helper.
    ///
    /// On Linux, routes through the per-handle `io_uring` ring when
    /// available. On `io_uring_setup(2)` rejection (cached on the
    /// Handle as `Disabled`), or when the ring path errors at
    /// runtime, falls through to the existing `O_DIRECT`+`pwrite`
    /// path. On macOS / Windows / unknown, always uses the existing
    /// platform `write_all_direct`.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::needless_pass_by_value, unused_imports)
    )]
    fn direct_write(&self, file: &std::fs::File, path: &Path, data: &[u8]) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            if let Some(ring) = self.io_uring_ring() {
                // Probe NVMe passthrough capability lazily on the
                // file fd's underlying block device. Returns None
                // for non-NVMe devices, missing privileges, or when
                // FSYS_DISABLE_NVME_PASSTHROUGH=1 is set.
                let nvme = self.nvme_access(file.as_raw_fd());
                let nvme_ref = nvme.as_deref();
                if iouring_write_direct(&ring, file, data, self.sector_size(), nvme_ref).is_ok() {
                    return Ok(());
                }
                // Ring submit failed at runtime — surface the
                // `pwrite` path's error so the caller observes a
                // single, comparable error class regardless of which
                // path produced it.
            }
        }
        let result = platform::write_all_direct(file, data, self.sector_size());

        // Windows: when NVMe passthrough is available, issue a
        // controller-level FLUSH after the WRITE_THROUGH write. This
        // is redundant durability (WRITE_THROUGH already flushes per
        // write) but exercises the IOCTL path and surfaces it via
        // `active_durability_primitive()`. Performance certification
        // (F-9) decides whether to drop WRITE_THROUGH when the
        // IOCTL is active.
        #[cfg(target_os = "windows")]
        if result.is_ok() {
            if let Some(access) = self.nvme_access_win(path) {
                // Best-effort: ignore NVMe FLUSH errors at runtime —
                // WRITE_THROUGH already provided durability. We log
                // at the metrics placeholder later (F-1) if/when the
                // metrics layer lands.
                let _ = crate::platform::windows_nvme::nvme_flush(&access);
            }
        }
        let _ = path; // path is unused on Linux; consumed on Windows above.
        result
    }

    /// Direct-IO read helper. Mirror of [`direct_write`].
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::needless_pass_by_value, unused_imports)
    )]
    fn direct_read(&self, file: &std::fs::File, file_size: u64) -> Result<Vec<u8>> {
        #[cfg(target_os = "linux")]
        {
            if let Some(ring) = self.io_uring_ring() {
                if let Ok(buf) = iouring_read_direct(&ring, file, file_size, self.sector_size()) {
                    return Ok(buf);
                }
            }
        }
        platform::read_all_direct(file, file_size, self.sector_size())
    }
}

#[cfg(target_os = "linux")]
fn iouring_write_direct(
    ring: &crate::platform::linux_iouring::IoUringRing,
    file: &std::fs::File,
    data: &[u8],
    sector_size: u32,
    nvme: Option<&crate::platform::linux_iouring::NvmeAccess>,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    // Empty input — skip the buffer-pool allocation entirely. The
    // file is already created at size 0 by the caller's `open()`;
    // we only need the durability fence below.
    if data.is_empty() {
        if let Some(access) = nvme {
            crate::platform::linux_iouring::nvme_flush_ioctl(
                access.char_dev.as_raw_fd(),
                access.nsid,
            )?;
        } else {
            ring.fdatasync(file.as_raw_fd())?;
        }
        return Ok(());
    }

    let ss = sector_size as usize;
    let aligned_len = data.len().div_ceil(ss).saturating_mul(ss);
    let mut buf = crate::platform::AlignedBuf::new(aligned_len, ss)?;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);

    // O_DIRECT minimises cache effects but does not imply durability.
    // The atomic-replace contract requires the bytes to be on stable
    // storage before the rename. Three paths:
    //
    // 1. NVMe passthrough flush (locked decision D-2). Sends NVMe
    //    FLUSH (opcode 0x00) directly to the controller via the
    //    legacy `NVME_IOCTL_IO_CMD` ioctl. Bypasses the kernel's
    //    fsync path entirely — the controller flushes its volatile
    //    write cache and acknowledges. Requires NVMe hardware +
    //    `CAP_SYS_ADMIN`-level access. Write and flush are
    //    submitted as separate calls because the NVMe FLUSH is an
    //    ioctl on a different fd (`/dev/nvmeX`), not an io_uring
    //    SQE — linking is not applicable.
    // 2. **0.9.4 linked write+fsync (`IOSQE_IO_LINK`).** When NVMe
    //    passthrough is unavailable, submit Write + Fsync(DATASYNC)
    //    as a linked SQE chain in one `io_uring_enter(2)` round-
    //    trip instead of two. Halves the syscall-entry cost of the
    //    durable-write path; kernel batches both completions in a
    //    single submit-and-wait.
    // 3. Standard fallback when the linked submission cannot be
    //    used (e.g. SQ queue full at link time): write + fdatasync
    //    as separate calls (the 0.5.1 path).
    if let Some(access) = nvme {
        let n = ring.write_at(file.as_raw_fd(), buf.as_slice(), 0)?;
        if n != aligned_len {
            return Err(Error::Io(std::io::Error::other(
                "io_uring short write on Direct path",
            )));
        }
        crate::platform::linux_iouring::nvme_flush_ioctl(access.char_dev.as_raw_fd(), access.nsid)?;
    } else {
        // 0.9.4: linked write + fsync. The owner thread pushes
        // both SQEs with IOSQE_IO_LINK and waits for both CQEs;
        // halves the durability syscall round-trip vs the
        // pre-0.9.4 two-submit path.
        let n = ring.write_at_linked_fsync(file.as_raw_fd(), buf.as_slice(), 0)?;
        if n != aligned_len {
            return Err(Error::Io(std::io::Error::other(
                "io_uring short write on Direct path (linked write+fsync)",
            )));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn iouring_read_direct(
    ring: &crate::platform::linux_iouring::IoUringRing,
    file: &std::fs::File,
    file_size: u64,
    sector_size: u32,
) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;

    if file_size == 0 {
        return Ok(Vec::new());
    }
    let ss = sector_size as usize;
    let len = file_size as usize;
    let aligned_len = len.div_ceil(ss).saturating_mul(ss);
    let mut buf = crate::platform::AlignedBuf::new(aligned_len, ss)?;

    let n = ring.read_at(file.as_raw_fd(), buf.as_mut_slice(), 0)?;
    if n < len {
        return Err(Error::Io(std::io::Error::other(
            "io_uring short read on Direct path",
        )));
    }
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&buf.as_slice()[..len]);
    Ok(out)
}

// Convert a `crate::Error` to a `std::io::Error` for use in
// `AtomicReplaceFailed { source: std::io::Error }`.
fn as_io_error(e: Error) -> std::io::Error {
    match e {
        Error::Io(io_err) => io_err,
        other => std::io::Error::other(other.to_string()),
    }
}

/// Applies the metadata-preservation set defined for `write_copy`
/// (locked decision D-8 in `.dev/DECISIONS-0.6.0.md`) to `staging`,
/// reading source attributes from `target` (the path whose metadata
/// we want to preserve) and `existing_meta`.
///
/// All operations are best-effort — on Unix, `chown` failures
/// (typically `EPERM` for non-root processes) are silently skipped
/// per the locked contract. Same logic on Windows for ACL
/// application.
fn apply_preserved_metadata(staging: &Path, target: &Path, existing: &std::fs::Metadata) {
    apply_preserved_metadata_inner(staging, target, existing);
}

#[cfg(unix)]
fn apply_preserved_metadata_inner(staging: &Path, _target: &Path, existing: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    // Mode — unconditional, identical bits as the existing file.
    let mode = existing.permissions().mode();
    let _ = std::fs::set_permissions(staging, std::fs::Permissions::from_mode(mode));

    // Owner / group — only succeeds when the process has CAP_CHOWN
    // or equivalent. Silently skipped on EPERM per D-8.
    let uid = existing.uid();
    let gid = existing.gid();
    if let Ok(c_path) = std::ffi::CString::new(staging.as_os_str().as_encoded_bytes()) {
        // SAFETY: c_path is a valid NUL-terminated path string built
        // from `staging`'s OsStr bytes. `chown(2)` returns an error
        // code on failure rather than panicking; we discard the
        // result to honour the silently-skip-on-EPERM contract from
        // D-8. No memory or aliasing invariants are at stake.
        let _ = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    }

    // Timestamps (mtime/atime). Best-effort.
    apply_timestamps_unix(staging, existing);
}

#[cfg(unix)]
fn apply_timestamps_unix(staging: &Path, existing: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;

    let atime = libc::timespec {
        tv_sec: existing.atime() as libc::time_t,
        tv_nsec: existing.atime_nsec(),
    };
    let mtime = libc::timespec {
        tv_sec: existing.mtime() as libc::time_t,
        tv_nsec: existing.mtime_nsec(),
    };
    let times = [atime, mtime];

    if let Ok(c_path) = std::ffi::CString::new(staging.as_os_str().as_encoded_bytes()) {
        // SAFETY: c_path is a valid NUL-terminated path string built
        // from `staging`'s OsStr bytes; `times` is a length-2 stack
        // array of `timespec` whose pointer remains valid for the
        // duration of the call; flag `0` means "follow symlinks".
        // utimensat returns an error code on failure rather than
        // panicking.
        let _ = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    }
}

#[cfg(windows)]
fn apply_preserved_metadata_inner(staging: &Path, target: &Path, existing: &std::fs::Metadata) {
    apply_timestamps_windows(staging, existing);
    // ACL preservation: copy the security descriptor from `target`
    // to `staging` via GetNamedSecurityInfoW / SetNamedSecurityInfoW.
    // Best-effort — failures are silent per D-8 (matching the
    // Unix chown-on-EPERM contract).
    apply_acls_windows(target, staging);
}

#[cfg(windows)]
fn apply_timestamps_windows(staging: &Path, existing: &std::fs::Metadata) {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, SetFileTime, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, OPEN_EXISTING,
    };

    let atime_u64 = existing.last_access_time();
    let mtime_u64 = existing.last_write_time();
    let ctime_u64 = existing.creation_time();

    let to_filetime = |t: u64| FILETIME {
        dwLowDateTime: (t & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (t >> 32) as u32,
    };
    let atime = to_filetime(atime_u64);
    let mtime = to_filetime(mtime_u64);
    let ctime = to_filetime(ctime_u64);

    let wide: Vec<u16> = staging
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path. CreateFileW
    // returns INVALID_HANDLE_VALUE on failure rather than
    // panicking; we check before using.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    // SAFETY: `h` is a valid handle from CreateFileW; ctime/atime/
    // mtime are valid FILETIME values; SetFileTime writes via the
    // pointers without retaining them.
    let _ = unsafe { SetFileTime(h, &ctime, &atime, &mtime) };
    // SAFETY: `h` was opened by CreateFileW above; CloseHandle is
    // the matching teardown.
    let _ = unsafe { CloseHandle(h) };
}

#[cfg(windows)]
fn apply_acls_windows(target: &Path, staging: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID,
    };

    let target_w: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let staging_w: Vec<u16> = staging
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut owner: PSID = std::ptr::null_mut();
    let mut group: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();

    let info_flags =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;

    // SAFETY: All output pointers are valid stack locations that
    // GetNamedSecurityInfoW writes to via raw pointer; the function
    // returns an error code on failure rather than panicking. The
    // returned `sd` must be freed with LocalFree (handled below).
    let rc = unsafe {
        GetNamedSecurityInfoW(
            target_w.as_ptr(),
            SE_FILE_OBJECT,
            info_flags,
            &mut owner,
            &mut group,
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 || sd.is_null() {
        return;
    }

    // SAFETY: `staging_w` is NUL-terminated UTF-16; owner/group/dacl
    // were populated by GetNamedSecurityInfoW above and remain valid
    // until we LocalFree(sd). SetNamedSecurityInfoW returns an error
    // code on failure rather than panicking.
    let _ = unsafe {
        SetNamedSecurityInfoW(
            staging_w.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            info_flags,
            owner,
            group,
            dacl,
            std::ptr::null_mut(),
        )
    };

    // SAFETY: `sd` was returned by GetNamedSecurityInfoW which docs
    // require LocalFree as the matching teardown.
    let _ = unsafe { LocalFree(sd as _) };
}

#[cfg(not(any(unix, windows)))]
fn apply_preserved_metadata_inner(_staging: &Path, _target: &Path, _existing: &std::fs::Metadata) {
    // Unsupported platform — no-op. The atomic-rename contract still
    // holds; we just don't preserve metadata.
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
        let chunk = h.read_at(&path, 3, 4).expect("read_at");
        assert_eq!(chunk, b"3456");
    }
}
