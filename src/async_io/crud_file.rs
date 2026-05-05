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

    /// Async variant of [`Handle::read_range`].
    pub async fn read_range_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.read_range(&path, offset, len))
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
