//! Public [`Batch`] builder for the group lane.
//!
//! Use the [`crate::Handle::batch`] method to construct a [`Batch`]
//! bound to a handle, then accumulate ops via chainable
//! [`Batch::write`] / [`Batch::delete`] / [`Batch::copy`] calls and
//! submit them all as one batch with [`Batch::commit`].
//!
//! ```no_run
//! # fn example() {
//! # let h = fsys::new().expect("build handle");
//! let mut batch = h.batch();
//! let _ = batch.write("a.bin", b"alpha");
//! let _ = batch.write("b.bin", b"beta");
//! let _ = batch.delete("stale.tmp");
//! batch.commit().expect("commit");
//! # }
//! ```
//!
//! ## When to use the builder vs `Handle::write_batch`
//!
//! - Use [`crate::Handle::write_batch`] when the batch is small or
//!   already collected into a slice. Zero builder overhead.
//! - Use [`Batch`] when the batch is large, dynamic, or built across
//!   many control-flow paths (e.g. inside a loop with conditional
//!   `write` / `delete` calls). Allocations are paced across each
//!   builder method call rather than concentrated at submission time
//!   — see decision R-15 in `.dev/DECISIONS-0.4.0.md`.
//!
//! Both paths produce identical durability and ordering semantics: ops
//! execute in submission order, the first failure stops the batch, and
//! ops that succeeded before the failure are durable.

use std::path::Path;

use crate::error::BatchError;
use crate::handle::Handle;
use crate::pipeline::BatchOp;

/// Group-lane batch builder bound to a [`crate::Handle`].
///
/// Created via [`Handle::batch`]. Accumulates ops in submission order
/// and submits them all when [`Batch::commit`] is called.
///
/// # Borrow / lifetime
///
/// `Batch<'a>` borrows the handle for the duration of the build →
/// commit window. The handle cannot be moved or dropped while a
/// `Batch` is outstanding; this is the borrow checker's natural
/// guarantee — if you have a `&mut Handle`, you cannot also have a
/// `Batch<'_>` borrowing it. (Solo-lane ops on the handle remain
/// available because they take `&self`, not `&mut self`.)
///
/// # Allocation semantics
///
/// Each call to [`Batch::write`] / [`Batch::delete`] / [`Batch::copy`]
/// allocates eagerly: the path is converted to a `PathBuf` and the
/// data is cloned into a `Vec<u8>` at the call site. This pacing is
/// intentional (decision R-15) — a 10K-op batch pays 10K small
/// allocations spread across the build loop, not one big burst at
/// commit.
#[must_use = "a Batch does nothing until committed; call .commit() or drop it explicitly"]
pub struct Batch<'a> {
    handle: &'a Handle,
    ops: Vec<BatchOp>,
}

impl<'a> Batch<'a> {
    /// Constructs a new empty batch bound to `handle`.
    ///
    /// `pub(crate)` — external callers obtain a `Batch` via
    /// [`Handle::batch`].
    pub(crate) fn new(handle: &'a Handle) -> Self {
        Self {
            handle,
            ops: Vec::new(),
        }
    }

    /// Queues an atomic write of `data` to `path`.
    ///
    /// The path and data are cloned at this call (R-15). Returns
    /// `&mut Self` for method chaining:
    ///
    /// ```ignore
    /// batch.write("a", b"alpha")
    ///      .write("b", b"beta")
    ///      .delete("stale");
    /// ```
    pub fn write<P: AsRef<Path>>(&mut self, path: P, data: &[u8]) -> &mut Self {
        self.ops.push(BatchOp::Write {
            path: path.as_ref().to_path_buf(),
            data: data.to_vec(),
        });
        self
    }

    /// Queues an idempotent delete of `path`.
    ///
    /// Missing files are not treated as failures (matches solo-lane
    /// [`Handle::delete`]).
    pub fn delete<P: AsRef<Path>>(&mut self, path: P) -> &mut Self {
        self.ops.push(BatchOp::Delete {
            path: path.as_ref().to_path_buf(),
        });
        self
    }

    /// Queues a copy of `src` to `dst`.
    pub fn copy<P: AsRef<Path>, Q: AsRef<Path>>(&mut self, src: P, dst: Q) -> &mut Self {
        self.ops.push(BatchOp::Copy {
            src: src.as_ref().to_path_buf(),
            dst: dst.as_ref().to_path_buf(),
        });
        self
    }

    /// Returns the number of accumulated ops.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Returns `true` if no ops have been accumulated yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Submits the accumulated batch through the group lane and blocks
    /// until the dispatcher reports completion.
    ///
    /// Path resolution against the handle's root happens here, not at
    /// each builder call. A batch that contains a path which escapes
    /// the handle root fails with `failed_at` set to the index of the
    /// offending op in the *built* sequence and `completed = 0` — no
    /// op was attempted.
    ///
    /// # Errors
    ///
    /// See [`Handle::write_batch`] for the full error contract.
    pub fn commit(self) -> std::result::Result<(), BatchError> {
        let Self { handle, ops } = self;
        let resolved = resolve_ops(handle, ops)?;
        handle.submit_batch(resolved)
    }

    /// 0.9.3 — **Grouped commit.** Same as [`Self::commit`] except
    /// the dispatcher amortises parent-directory `fsync` calls
    /// across the entire batch instead of paying one per op.
    ///
    /// **The mechanic.** Each op still does its own data
    /// `fdatasync` (the temp file's content is durable before the
    /// rename — that's required for atomic-replace correctness).
    /// What changes is the **post-rename** `sync_parent_dir` step:
    /// the regular path calls it once per op, the grouped path
    /// accumulates unique parent directories and calls it once per
    /// unique parent after the entire batch succeeds.
    ///
    /// **The numbers.** A typical "flush 1024 SST files into one
    /// directory" batch pays 1024 `sync_parent_dir` syscalls under
    /// [`Self::commit`] and exactly **one** under
    /// [`Self::commit_grouped`]. On Linux/macOS each
    /// `sync_parent_dir` is a real `fsync` on the directory file
    /// descriptor (microseconds to milliseconds depending on
    /// filesystem and dirty-page load). On Windows the call is a
    /// no-op (directory durability is implicit), so this method
    /// is observably equivalent to [`Self::commit`] on Windows.
    ///
    /// **The trade-off.** Under regular `commit`, every op is
    /// individually durable on return (including its dirent
    /// update). Under `commit_grouped`, ops are durable *as a
    /// set* on return — a crash mid-batch may leave a prefix of
    /// the renames visible while the dirent updates have not yet
    /// landed on the journal. The per-op data is on disk
    /// regardless (its data fsync still happened), so on the
    /// next fsync/reboot the filesystem journal replays the
    /// dirent updates. Callers that need per-op dirent
    /// durability (rare — usually only when each op IS a
    /// transaction commit) should use [`Self::commit`].
    ///
    /// **When to use it.** Bulk loads, SST flushes, database
    /// checkpoint emissions — any workload where the batch is
    /// the durability unit, not the individual op.
    ///
    /// # Errors
    ///
    /// Same contract as [`Self::commit`].
    pub fn commit_grouped(self) -> std::result::Result<(), BatchError> {
        let Self { handle, ops } = self;
        let resolved = resolve_ops(handle, ops)?;
        handle.submit_batch_grouped(resolved)
    }
}

/// Resolves every op's path(s) against the handle's root, returning
/// a fresh vec of resolved [`BatchOp`]s ready for submission. On the
/// first path-resolution failure, returns a [`BatchError`] with
/// `failed_at` = the offending op's index, `completed = 0` — no op
/// has been dispatched yet. Shared by [`Batch::commit`] and
/// [`Batch::commit_grouped`].
fn resolve_ops(
    handle: &Handle,
    ops: Vec<BatchOp>,
) -> std::result::Result<Vec<BatchOp>, BatchError> {
    let mut resolved: Vec<BatchOp> = Vec::with_capacity(ops.len());
    for (i, op) in ops.into_iter().enumerate() {
        match op {
            BatchOp::Write { path, data } => {
                let p = handle
                    .resolve_path(&path)
                    .map_err(|e| pre_submit_err(i, e))?;
                resolved.push(BatchOp::Write { path: p, data });
            }
            BatchOp::Delete { path } => {
                let p = handle
                    .resolve_path(&path)
                    .map_err(|e| pre_submit_err(i, e))?;
                resolved.push(BatchOp::Delete { path: p });
            }
            BatchOp::Copy { src, dst } => {
                let s = handle
                    .resolve_path(&src)
                    .map_err(|e| pre_submit_err(i, e))?;
                let d = handle
                    .resolve_path(&dst)
                    .map_err(|e| pre_submit_err(i, e))?;
                resolved.push(BatchOp::Copy { src: s, dst: d });
            }
        }
    }
    Ok(resolved)
}

fn pre_submit_err(index: usize, e: crate::Error) -> BatchError {
    BatchError {
        failed_at: index,
        completed: 0,
        source: Box::new(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::Builder;
    use crate::method::Method;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_batch_test_{}_{}_{}",
            std::process::id(),
            n,
            suffix
        ))
    }

    fn handle() -> Handle {
        Builder::new()
            .method(Method::Sync)
            .build()
            .expect("build handle")
    }

    struct TmpFile(PathBuf);
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_new_batch_is_empty() {
        let h = handle();
        let b = h.batch();
        assert_eq!(b.len(), 0);
        assert!(b.is_empty());
    }

    #[test]
    fn test_write_appends_op() {
        let h = handle();
        let mut b = h.batch();
        let _ = b.write("/tmp/x", b"y");
        assert_eq!(b.len(), 1);
        assert!(!b.is_empty());
    }

    #[test]
    fn test_chained_calls_accumulate() {
        let h = handle();
        let mut b = h.batch();
        let _ = b.write("a", b"1").write("b", b"2").delete("c");
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn test_commit_executes_writes_in_order() {
        let h = handle();
        let p = tmp_path("commit_order");
        let _g = TmpFile(p.clone());
        let mut b = h.batch();
        let _ = b
            .write(&p, b"first")
            .write(&p, b"second")
            .write(&p, b"third");
        b.commit().expect("commit");
        // Strict input order → last write wins.
        assert_eq!(std::fs::read(&p).unwrap(), b"third");
    }

    #[test]
    fn test_commit_executes_mixed_ops() {
        let h = handle();
        let written = tmp_path("mixed_written");
        let copied_from = tmp_path("mixed_copy_src");
        let copied_to = tmp_path("mixed_copy_dst");
        let to_delete = tmp_path("mixed_delete");
        let _g1 = TmpFile(written.clone());
        let _g2 = TmpFile(copied_from.clone());
        let _g3 = TmpFile(copied_to.clone());
        let _g4 = TmpFile(to_delete.clone());

        std::fs::write(&copied_from, b"src-data").unwrap();
        std::fs::write(&to_delete, b"existing").unwrap();

        let mut b = h.batch();
        let _ = b
            .write(&written, b"freshly-written")
            .copy(&copied_from, &copied_to)
            .delete(&to_delete);
        b.commit().expect("commit");

        assert_eq!(std::fs::read(&written).unwrap(), b"freshly-written");
        assert_eq!(std::fs::read(&copied_to).unwrap(), b"src-data");
        assert!(!to_delete.exists());
    }

    #[test]
    fn test_commit_empty_batch_succeeds() {
        let h = handle();
        let b = h.batch();
        b.commit().expect("empty batch should succeed");
    }

    #[test]
    fn test_commit_reports_failure_index_for_op_error() {
        let h = handle();
        let good = tmp_path("good_op");
        let bad_dir = tmp_path("bad_dir_op");
        let _g1 = TmpFile(good.clone());
        std::fs::create_dir_all(&bad_dir).unwrap();
        struct DirGuard(PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _g2 = DirGuard(bad_dir.clone());

        let mut b = h.batch();
        let _ = b
            .write(&good, b"ok")
            .write(&bad_dir, b"this-must-fail-target-is-dir")
            .write(&good, b"never-reached");
        let err = b.commit().expect_err("expected failure on op 1");
        assert_eq!(err.failed_at, 1);
        assert_eq!(err.completed, 1);
        assert_eq!(std::fs::read(&good).unwrap(), b"ok");
    }

    #[test]
    fn test_batch_holds_borrow_to_handle() {
        // Lifetime check: Batch<'a> cannot outlive the handle.
        let h = handle();
        let mut b = h.batch();
        let _ = b.write("x", b"y");
        // If `Batch<'a>` did not borrow the handle, the next line would
        // allow moving `h` while `b` is alive — this test confirms the
        // borrow exists by exercising the typical pattern.
        let len = b.len();
        assert_eq!(len, 1);
        // Drop b before h.
        drop(b);
        drop(h);
    }

    #[test]
    fn test_batch_must_use_attribute_documented() {
        // This is a compile-only guarantee enforced by `#[must_use]` on
        // the struct. The test exists as documentation that the
        // attribute is intentional.
        let h = handle();
        let _ = h.batch(); // explicit drop is fine
    }

    #[test]
    fn test_path_resolution_failure_reports_correct_index() {
        // Build a handle with a root and submit a batch where one op's
        // path escapes the root.
        let root = std::env::temp_dir().join(format!(
            "fsys_batch_root_{}_{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        struct DirGuard(PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _rg = DirGuard(root.clone());

        let h = Builder::new()
            .root(&root)
            .method(Method::Sync)
            .build()
            .expect("build with root");

        let mut b = h.batch();
        let _ = b
            .write("ok-1", b"a")
            .write("../../etc/passwd", b"escape")
            .write("ok-2", b"b");
        let err = b.commit().expect_err("escape must be rejected");
        assert_eq!(err.failed_at, 1);
        assert_eq!(err.completed, 0); // nothing dispatched yet
        match *err.source {
            crate::Error::InvalidPath { .. } => { /* expected */ }
            ref other => panic!("expected InvalidPath, got {:?}", other),
        }
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.3 — Batch::commit_grouped() coverage
    // ─────────────────────────────────────────────────────────

    #[test]
    fn test_commit_grouped_executes_writes_in_order() {
        let h = handle();
        let p = tmp_path("grouped_order");
        let _g = TmpFile(p.clone());
        let mut b = h.batch();
        let _ = b
            .write(&p, b"first")
            .write(&p, b"second")
            .write(&p, b"third");
        b.commit_grouped().expect("commit_grouped");
        // Same strict-order contract as commit() — last write wins.
        assert_eq!(std::fs::read(&p).unwrap(), b"third");
    }

    #[test]
    fn test_commit_grouped_mixed_ops_into_one_directory() {
        // Common shape: many writes to files under one parent
        // directory. The grouped path should issue exactly one
        // sync_parent_dir; we can't easily assert that on
        // Windows (no-op), but we can verify all files land
        // and the call succeeds.
        let h = handle();
        let a = tmp_path("grouped_a");
        let bp = tmp_path("grouped_b");
        let c = tmp_path("grouped_c");
        let _ga = TmpFile(a.clone());
        let _gb = TmpFile(bp.clone());
        let _gc = TmpFile(c.clone());

        let mut b = h.batch();
        let _ = b
            .write(&a, b"alpha")
            .write(&bp, b"bravo")
            .write(&c, b"charlie");
        b.commit_grouped().expect("commit_grouped mixed");

        assert_eq!(std::fs::read(&a).unwrap(), b"alpha");
        assert_eq!(std::fs::read(&bp).unwrap(), b"bravo");
        assert_eq!(std::fs::read(&c).unwrap(), b"charlie");
    }

    #[test]
    fn test_commit_grouped_empty_batch_succeeds() {
        let h = handle();
        let b = h.batch();
        b.commit_grouped()
            .expect("empty grouped batch should succeed");
    }

    #[test]
    fn test_commit_grouped_reports_failure_index() {
        // The grouped path preserves the same error contract as
        // commit(): on the first failure, no further ops are
        // attempted and the post-batch parent-dir sync is also
        // skipped (we never reach a "successful" state).
        let h = handle();
        let good = tmp_path("grouped_good");
        let bad_dir = tmp_path("grouped_bad_dir");
        let _g1 = TmpFile(good.clone());
        std::fs::create_dir_all(&bad_dir).unwrap();
        struct DirGuard(PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _g2 = DirGuard(bad_dir.clone());

        let mut b = h.batch();
        let _ = b
            .write(&good, b"ok")
            .write(&bad_dir, b"target-is-dir-must-fail")
            .write(&good, b"never-reached");
        let err = b.commit_grouped().expect_err("expected failure on op 1");
        assert_eq!(err.failed_at, 1);
        assert_eq!(err.completed, 1);
        assert_eq!(std::fs::read(&good).unwrap(), b"ok");
    }

    #[test]
    fn test_commit_grouped_path_resolution_failure() {
        // Path-escape rejection happens before any dispatch, so
        // it returns the same pre-submit error shape as commit().
        let root = std::env::temp_dir().join(format!(
            "fsys_batch_grouped_root_{}_{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        struct DirGuard(PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _rg = DirGuard(root.clone());

        let h = Builder::new()
            .root(&root)
            .method(Method::Sync)
            .build()
            .expect("build with root");

        let mut b = h.batch();
        let _ = b
            .write("ok-1", b"a")
            .write("../../etc/passwd", b"escape")
            .write("ok-2", b"b");
        let err = b
            .commit_grouped()
            .expect_err("escape must be rejected in grouped mode too");
        assert_eq!(err.failed_at, 1);
        assert_eq!(err.completed, 0);
    }
}
