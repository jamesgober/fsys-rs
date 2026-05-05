//! Async batch operations.
//!
//! Unlike the single-op `_async` methods (which use
//! [`tokio::task::spawn_blocking`]), the batch async path goes
//! through the same per-handle dispatcher as sync batches but
//! responds via [`tokio::sync::oneshot`] — locked decision D-5 in
//! `.dev/DECISIONS-0.6.0.md`. The caller `.await`s the oneshot
//! without blocking a tokio worker.
//!
//! ## Why not `spawn_blocking`?
//!
//! The dispatcher is already a single per-handle thread that
//! serialises batch processing. Wrapping batch submission in
//! `spawn_blocking` would needlessly hop through a second thread
//! pool. The oneshot path is one fewer thread hop and integrates
//! cleanly with the existing dispatcher.

use crate::error::BatchError;
use crate::handle::Handle;
use crate::pipeline::BatchOp;
use std::sync::Arc;

impl Handle {
    /// Async variant of [`Handle::write_batch`].
    ///
    /// Submits the batch to the per-handle dispatcher and `.await`s
    /// completion via a [`tokio::sync::oneshot`] channel. The
    /// dispatcher itself is shared with sync batches; only the
    /// response path differs.
    ///
    /// # Errors
    ///
    /// Same shape as [`Handle::write_batch`]:
    /// [`BatchError`] wrapping the underlying [`crate::Error`] for
    /// the failing op (or [`crate::Error::ShutdownInProgress`] when
    /// the handle is being dropped).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> fsys::Result<()> {
    /// # use std::path::Path;
    /// let fs = std::sync::Arc::new(fsys::builder().build()?);
    /// let writes: Vec<(std::path::PathBuf, Vec<u8>)> = vec![
    ///     ("a.dat".into(), b"alpha".to_vec()),
    ///     ("b.dat".into(), b"beta".to_vec()),
    /// ];
    /// fs.clone().write_batch_async(writes).await
    ///     .map_err(|be| fsys::Error::Io(std::io::Error::other(be.to_string())))?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn write_batch_async(
        self: Arc<Self>,
        batch: Vec<(std::path::PathBuf, Vec<u8>)>,
    ) -> std::result::Result<(), BatchError> {
        let mut ops: Vec<BatchOp> = Vec::with_capacity(batch.len());
        for (i, (path, data)) in batch.into_iter().enumerate() {
            let resolved = self.resolve_path(&path).map_err(|e| BatchError {
                failed_at: i,
                completed: 0,
                source: Box::new(e),
            })?;
            ops.push(BatchOp::Write {
                path: resolved,
                data,
            });
        }
        self.submit_batch_async(ops).await
    }

    /// Async variant of [`Handle::delete_batch`].
    pub async fn delete_batch_async(
        self: Arc<Self>,
        batch: Vec<std::path::PathBuf>,
    ) -> std::result::Result<(), BatchError> {
        let mut ops: Vec<BatchOp> = Vec::with_capacity(batch.len());
        for (i, path) in batch.into_iter().enumerate() {
            let resolved = self.resolve_path(&path).map_err(|e| BatchError {
                failed_at: i,
                completed: 0,
                source: Box::new(e),
            })?;
            ops.push(BatchOp::Delete { path: resolved });
        }
        self.submit_batch_async(ops).await
    }

    /// Async variant of [`Handle::copy_batch`].
    pub async fn copy_batch_async(
        self: Arc<Self>,
        batch: Vec<(std::path::PathBuf, std::path::PathBuf)>,
    ) -> std::result::Result<(), BatchError> {
        let mut ops: Vec<BatchOp> = Vec::with_capacity(batch.len());
        for (i, (src, dst)) in batch.into_iter().enumerate() {
            let resolved_src = self.resolve_path(&src).map_err(|e| BatchError {
                failed_at: i,
                completed: 0,
                source: Box::new(e),
            })?;
            let resolved_dst = self.resolve_path(&dst).map_err(|e| BatchError {
                failed_at: i,
                completed: 0,
                source: Box::new(e),
            })?;
            ops.push(BatchOp::Copy {
                src: resolved_src,
                dst: resolved_dst,
            });
        }
        self.submit_batch_async(ops).await
    }
}
