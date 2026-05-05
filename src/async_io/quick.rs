//! Async wrappers for the `fsys::quick::*` one-shot helpers.
//!
//! Each function here is a thin [`tokio::task::spawn_blocking`] over
//! the corresponding sync function in [`crate::quick`]. Useful when
//! the caller doesn't have (or doesn't need) a long-lived
//! [`crate::Handle`] and just wants `await`-shaped one-shot IO.
//!
//! For repeated IO from inside an async context, prefer constructing
//! a [`crate::Handle`] once and using `Handle::*_async` — that avoids
//! the per-call `default_handle` lookup overhead from `quick::*`.

use crate::method::Method;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Async variant of [`crate::quick::write`].
///
/// # Errors
///
/// Returns [`Error::AsyncRuntimeRequired`] when called outside a
/// tokio runtime. Otherwise propagates [`crate::quick::write`].
///
/// # Examples
///
/// ```no_run
/// # async fn example() -> fsys::Result<()> {
/// fsys::async_io::quick::write_async("/tmp/foo", b"bar".to_vec()).await?;
/// # Ok(())
/// # }
/// ```
pub async fn write_async(path: impl AsRef<Path>, data: Vec<u8>) -> Result<()> {
    super::require_runtime()?;
    let path: PathBuf = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || crate::quick::write(&path, &data))
        .await
        .map_err(join_error_to_io)?
}

/// Async variant of [`crate::quick::write_with`].
pub async fn write_with_async(path: impl AsRef<Path>, data: Vec<u8>, method: Method) -> Result<()> {
    super::require_runtime()?;
    let path: PathBuf = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || crate::quick::write_with(&path, &data, method))
        .await
        .map_err(join_error_to_io)?
}

/// Async variant of [`crate::quick::read`].
pub async fn read_async(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    super::require_runtime()?;
    let path: PathBuf = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || crate::quick::read(&path))
        .await
        .map_err(join_error_to_io)?
}

/// Async variant of [`crate::quick::delete`].
pub async fn delete_async(path: impl AsRef<Path>) -> Result<()> {
    super::require_runtime()?;
    let path: PathBuf = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || crate::quick::delete(&path))
        .await
        .map_err(join_error_to_io)?
}

fn join_error_to_io(e: tokio::task::JoinError) -> Error {
    Error::Io(std::io::Error::other(format!(
        "spawn_blocking task failed: {e}"
    )))
}
