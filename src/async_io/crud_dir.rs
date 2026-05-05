//! Async directory CRUD wrappers. See [`crate::async_io`] for the
//! design rationale.

use crate::handle::Handle;
use crate::meta::DirEntry;
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl Handle {
    /// Async variant of [`Handle::mkdir`].
    pub async fn mkdir_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.mkdir(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::mkdir_all`].
    pub async fn mkdir_all_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.mkdir_all(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::rmdir`].
    pub async fn rmdir_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.rmdir(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::rmdir_all`].
    pub async fn rmdir_all_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<()> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.rmdir_all(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::list`].
    pub async fn list_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<Vec<DirEntry>> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.list(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::scan`].
    pub async fn scan_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        recursive: bool,
    ) -> Result<Vec<DirEntry>> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.scan(&path, recursive))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::find`].
    pub async fn find_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        pattern: impl Into<String>,
    ) -> Result<Vec<PathBuf>> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        let pattern: String = pattern.into();
        tokio::task::spawn_blocking(move || self.find(&path, &pattern))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::count`].
    pub async fn count_async(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        recursive: bool,
    ) -> Result<usize> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.count(&path, recursive))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::is_dir`].
    pub async fn is_dir_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<bool> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.is_dir(&path))
            .await
            .map_err(join_error_to_io)?
    }

    /// Async variant of [`Handle::is_file`].
    pub async fn is_file_async(self: Arc<Self>, path: impl AsRef<Path>) -> Result<bool> {
        super::require_runtime()?;
        let path: PathBuf = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || self.is_file(&path))
            .await
            .map_err(join_error_to_io)?
    }
}

fn join_error_to_io(e: tokio::task::JoinError) -> Error {
    Error::Io(std::io::Error::other(format!(
        "spawn_blocking task failed: {e}"
    )))
}
