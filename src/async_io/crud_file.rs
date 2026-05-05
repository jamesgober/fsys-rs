//! Async file CRUD wrappers.
//!
//! Each method here is a thin [`tokio::task::spawn_blocking`] over
//! the corresponding sync method on [`crate::Handle`]. See
//! [`crate::async_io`] for the design rationale.

use crate::handle::Handle;
use crate::meta::FileMeta;
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl Handle {
    /// Async variant of [`Handle::write`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::AsyncRuntimeRequired`] when called outside a
    /// tokio runtime, otherwise propagates the same errors as
    /// [`Handle::write`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> fsys::Result<()> {
    /// let fs = std::sync::Arc::new(fsys::builder().build()?);
    /// fs.clone().write_async("data.bin", b"payload".to_vec()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn write_async(self: Arc<Self>, path: impl AsRef<Path>, data: Vec<u8>) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();

        // Native io_uring substrate path (Linux + Direct + ring
        // available + no env override). Routes the write +
        // fdatasync hot path through native io_uring, bypassing
        // the spawn_blocking thread-pool hop. Open + rename stay
        // synchronous on the calling task — they're sub-µs and
        // not worth the async machinery.
        #[cfg(target_os = "linux")]
        {
            if self.active_method() == crate::Method::Direct
                && std::env::var_os("FSYS_DISABLE_NATIVE_ASYNC").is_none()
            {
                if let Some(ring) = self.async_io_uring() {
                    return write_async_native(&self, &ring, &path, &data).await;
                }
            }
        }

        // spawn_blocking fallback — every other configuration.
        tokio::task::spawn_blocking(move || self.write(&path, &data))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::write_at`].
    ///
    /// # Errors
    ///
    /// Same as [`Handle::write_at`], plus [`Error::AsyncRuntimeRequired`].
    pub async fn write_at_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.write_at(&path, offset, &data))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::write_copy`].
    ///
    /// # Errors
    ///
    /// Same as [`Handle::write_copy`], plus [`Error::AsyncRuntimeRequired`].
    pub async fn write_copy_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        data: Vec<u8>,
    ) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.write_copy(&path, &data))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::append`].
    pub async fn append_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        data: Vec<u8>,
    ) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.append(&path, &data))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::read`].
    pub async fn read_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.read(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::read_at`].
    ///
    /// Renamed from `read_range_async` in `0.7.0` per the API
    /// audit (matches the sync `Handle::read_at` rename). The
    /// old name is **removed**, not deprecated, because the
    /// audit landed before the alpha freeze.
    pub async fn read_at_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.read_at(&path, offset, len))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::delete`].
    pub async fn delete_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.delete(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::truncate`].
    pub async fn truncate_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        new_size: u64,
    ) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.truncate(&path, new_size))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::rename`].
    pub async fn rename_async(
        self: Arc<Self>,
        old: impl AsRef<Path>,
        new: impl AsRef<Path>,
    ) -> Result<()> {
        super::require_runtime()?;
        let old: PathBuf = old.as_ref().to_path_buf();
        let new: PathBuf = new.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.rename(&old, &new))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::copy`].
    pub async fn copy_async(
        self: Arc<Self>,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<u64> {
        super::require_runtime()?;
        let src: PathBuf = src.as_ref().to_path_buf();
        let dst: PathBuf = dst.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.copy(&src, &dst))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::exists`].
    pub async fn exists_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<bool> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.exists(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::size`].
    pub async fn size_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<u64> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.size(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::meta`].
    pub async fn meta_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<FileMeta> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.meta(&path))
            .await
            .map_err(join_error_to_io)?
    }
}

fn join_error_to_io(e: tokio::task::JoinError) -> Error {
    Error::Io(std::io::Error::other(format!(
        "spawn_blocking task failed: {e}"
    )))
}

/// Native io_uring `write_async` implementation. Routes the inner
/// write + fdatasync through the per-handle native substrate; open
/// and rename stay on the calling task as direct sync calls (they
/// are sub-µs and not worth the async machinery).
///
/// This is a Linux-only path and only invoked when the substrate
/// has been confirmed `NativeIoUring`. Failures here surface as
/// `Error::AtomicReplaceFailed { step, source }` matching the
/// sync `Handle::write` shape — callers cannot distinguish native
/// vs spawn_blocking errors by variant.
#[cfg(target_os = "linux")]
async fn write_async_native(
    handle: &Handle,
    ring: &crate::async_io::completion_driver::AsyncIoUring,
    path: &Path,
    data: &[u8],
) -> Result<()> {
    use crate::async_io::iouring_substrate::{fdatasync_native, write_at_native};
    use std::os::fd::AsRawFd;

    // Resolve path against handle root (rejects escapes).
    let resolved = handle.resolve_path(path)?;
    let temp = Handle::gen_temp_path(&resolved);

    // Open temp file synchronously. The Direct path uses
    // O_DIRECT when supported; we don't try to open async because
    // tokio's fs::File doesn't support O_DIRECT cleanly and the
    // open syscall is sub-µs anyway.
    let (file, direct_ok) =
        crate::platform::open_write_new(&temp, handle.use_direct()).map_err(|e| {
            Error::AtomicReplaceFailed {
                step: "open_temp",
                source: as_io_error(e),
            }
        })?;

    if handle.use_direct() && !direct_ok {
        // O_DIRECT was rejected by the filesystem (tmpfs etc.).
        // Update the handle's active method so the next call falls
        // through the spawn_blocking branch in `write_async`. For
        // THIS op, surface the rejection as an error — the caller
        // can retry, and the retry will not take the native path.
        handle.update_active_method(crate::Method::Data);
        drop(file);
        let _ = std::fs::remove_file(&temp);
        return Err(Error::AtomicReplaceFailed {
            step: "open_temp",
            source: std::io::Error::other(
                "O_DIRECT unsupported on this filesystem; active_method downgraded to Data — retry",
            ),
        });
    }

    // Compute aligned length for Direct IO.
    let sector_size = handle.sector_size() as usize;
    let aligned_len = data.len().div_ceil(sector_size).saturating_mul(sector_size);
    let mut buf = crate::platform::AlignedBuf::new(aligned_len, sector_size).map_err(|e| {
        Error::AtomicReplaceFailed {
            step: "alloc_aligned_buf",
            source: as_io_error(e),
        }
    })?;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);

    // Native write: io_uring SQE for IORING_OP_WRITE.
    let n = match write_at_native(ring, file.as_raw_fd(), buf.as_slice(), 0).await {
        Ok(n) => n,
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(&temp);
            return Err(Error::AtomicReplaceFailed {
                step: "write_native",
                source: as_io_error(e),
            });
        }
    };
    if n != aligned_len {
        drop(file);
        let _ = std::fs::remove_file(&temp);
        return Err(Error::AtomicReplaceFailed {
            step: "write_native_short",
            source: std::io::Error::other("native io_uring write returned short count"),
        });
    }

    // Native fdatasync: io_uring SQE for IORING_OP_FSYNC + DATASYNC.
    if let Err(e) = fdatasync_native(ring, file.as_raw_fd()).await {
        drop(file);
        let _ = std::fs::remove_file(&temp);
        return Err(Error::AtomicReplaceFailed {
            step: "fdatasync_native",
            source: as_io_error(e),
        });
    }

    // Drop the O_DIRECT fd. Reopen buffered solely to truncate to
    // the actual data length (matching the sync Direct path).
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

    // Atomic rename — synchronous, sub-µs.
    if let Err(e) = crate::platform::atomic_rename(&temp, &resolved) {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::AtomicReplaceFailed {
            step: "rename",
            source: as_io_error(e),
        });
    }

    let _ = crate::platform::sync_parent_dir(&resolved);
    Ok(())
}

#[cfg(target_os = "linux")]
fn as_io_error(e: Error) -> std::io::Error {
    match e {
        Error::Io(io_err) => io_err,
        other => std::io::Error::other(other.to_string()),
    }
}
