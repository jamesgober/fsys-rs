//! Async file CRUD wrappers.
//!
//! Each method here is a thin [`tokio::task::spawn_blocking`] over
//! the corresponding sync method on [`crate::Handle`], except
//! `write_async` on Linux with `Method::Direct`, which runs the same
//! atomic-replace sequence through the native io_uring substrate. See
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
        // fdatasync through native io_uring, bypassing the
        // spawn_blocking thread-pool hop. Open + rename stay
        // synchronous on the calling task.
        #[cfg(target_os = "linux")]
        {
            if self.active_method() == crate::Method::Direct && !super::native_async_disabled() {
                if let Some(ring) = self.async_io_uring() {
                    return write_async_native(&self, &ring, &path, data).await;
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

/// Native io_uring `write_async` implementation, following the same
/// atomic-replace sequence as the sync [`Handle::write`]:
///
/// 1. Open a temp file next to the target (`O_DIRECT` when the
///    handle uses it). If the filesystem rejects `O_DIRECT` (tmpfs,
///    some FUSE mounts), the active method drops to
///    [`crate::Method::Data`] and this write continues buffered,
///    exactly like the sync path.
/// 2. Write the payload through the ring. The buffer and a
///    keep-alive for the temp file move into the driver (see
///    `completion_driver`), so cancellation cannot free memory the
///    kernel is still using.
/// 3. For `O_DIRECT`, trim the sector padding with `set_len` on the
///    same handle.
/// 4. `fdatasync` through the ring. This fence covers the data and
///    the final file size, and it completes before the rename.
/// 5. Rename over the target, then sync the parent directory
///    (best-effort, as in the sync path).
///
/// A guard removes the temp file on every early return and when the
/// future is dropped before the rename, so a cancelled `write_async`
/// leaves the target untouched and no `.fsys-tmp-*` file behind.
/// Open and rename stay synchronous on the calling task.
///
/// Failures surface as `Error::AtomicReplaceFailed { step, source }`
/// matching the sync `Handle::write` shape.
#[cfg(target_os = "linux")]
async fn write_async_native(
    handle: &Handle,
    ring: &crate::async_io::completion_driver::AsyncIoUring,
    path: &Path,
    data: Vec<u8>,
) -> Result<()> {
    use crate::async_io::completion_driver::{FileRef, IoBuf};
    use crate::async_io::iouring_substrate::{fdatasync_native, write_at_native};
    use std::os::fd::AsRawFd;

    let failed = |step: &'static str| {
        move |e: Error| Error::AtomicReplaceFailed {
            step,
            source: as_io_error(e),
        }
    };

    // Resolve path against handle root (rejects escapes).
    let resolved = handle.resolve_path(path)?;
    let temp = Handle::gen_temp_path(&resolved);

    let (file, direct_ok) =
        crate::platform::open_write_new(&temp, handle.use_direct()).map_err(failed("open_temp"))?;
    let mut temp_guard = TempFileGuard {
        path: &temp,
        armed: true,
    };
    if handle.use_direct() && !direct_ok {
        handle.update_active_method(crate::Method::Data);
    }

    // The driver holds a clone of this `Arc` as the fd keep-alive
    // while an op is in flight.
    let file = Arc::new(file);
    let file_ref = || FileRef::new(Arc::clone(&file), |f| f.as_raw_fd());

    let data_len = data.len();
    if data_len > 0 {
        let (buf, submit_len) = if direct_ok {
            let sector_size = handle.sector_size() as usize;
            let aligned_len = data_len.div_ceil(sector_size).saturating_mul(sector_size);
            let mut buf = crate::platform::AlignedBuf::new(aligned_len, sector_size)
                .map_err(failed("alloc_aligned_buf"))?;
            buf.as_mut_slice()[..data_len].copy_from_slice(&data);
            drop(data);
            (IoBuf::Aligned(buf), aligned_len)
        } else {
            (IoBuf::Vec(data), data_len)
        };
        let written = write_at_native(ring, file_ref(), buf, 0)
            .await
            .map_err(failed("write_native"))?;
        if written != submit_len {
            return Err(Error::AtomicReplaceFailed {
                step: "write_native_short",
                source: std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "native io_uring write made no progress before the end of the payload",
                ),
            });
        }
        if submit_len != data_len {
            // `O_DIRECT` wrote whole sectors; trim the padding on the
            // same handle (ftruncate) before the durability fence.
            file.set_len(data_len as u64)
                .map_err(|source| Error::AtomicReplaceFailed {
                    step: "truncate",
                    source,
                })?;
        }
    }

    // Durability fence: data and size are on stable storage before
    // the rename publishes the file.
    fdatasync_native(ring, file_ref())
        .await
        .map_err(failed("fdatasync_native"))?;
    drop(file);

    crate::platform::atomic_rename(&temp, &resolved).map_err(failed("rename"))?;
    temp_guard.armed = false;

    // Best-effort, as in the sync path: the rename already happened,
    // so a failed directory sync cannot be undone or retried here.
    let _ = crate::platform::sync_parent_dir(&resolved);
    Ok(())
}

/// Removes a `write_async` temp file unless the write reached its
/// rename. Runs on early returns and when the future is dropped
/// mid-write.
#[cfg(target_os = "linux")]
struct TempFileGuard<'a> {
    path: &'a Path,
    armed: bool,
}

#[cfg(target_os = "linux")]
impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort, like the sync path: the caller sees the
            // original error (or nothing, if the future was
            // cancelled); a leftover temp file is the only cost of a
            // failed unlink. An in-flight write to the unlinked inode
            // is harmless.
            let _ = std::fs::remove_file(self.path);
        }
    }
}

#[cfg(target_os = "linux")]
fn as_io_error(e: Error) -> std::io::Error {
    match e {
        Error::Io(io_err) => io_err,
        other => std::io::Error::other(other.to_string()),
    }
}
