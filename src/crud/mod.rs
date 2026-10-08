//! CRUD operations on files and directories.
//!
//! All operations are implemented as `impl Handle` blocks:
//! - `file`: file write, read, append, delete, copy, exists, size, metadata.
//! - `dir`: directory create, remove, list, exists.
//!
//! `atomic` holds the temp-file + rename sequence shared by every
//! whole-file write path (solo lane and group lane).

pub(crate) mod atomic;
pub(crate) mod dir;
pub(crate) mod file;

/// Data-level durability fence (`fdatasync` on Linux, `F_FULLFSYNC` on
/// macOS, `FlushFileBuffers` on Windows) used by the atomic-replace
/// paths before the rename publishes a temp file.
///
/// Routed through one function so the unit tests can confirm that a
/// fence actually ran on a given code path (see `fence_probe`).
#[inline]
pub(crate) fn fence_data(file: &std::fs::File) -> crate::Result<()> {
    #[cfg(test)]
    fence_probe::record_fence();
    crate::platform::sync_data(file)
}

/// Full durability fence (`fsync` on Linux, `F_FULLFSYNC` on macOS,
/// `FlushFileBuffers` on Windows): data and all inode metadata (mode,
/// owner, timestamps). Counted by `fence_probe` like [`fence_data`].
#[inline]
pub(crate) fn fence_full(file: &std::fs::File) -> crate::Result<()> {
    #[cfg(test)]
    fence_probe::record_fence();
    crate::platform::sync_full(file)
}

/// Makes a directory-entry change (rename, create, unlink) under the
/// parent of `path` durable: `fsync` on the parent directory on
/// Linux / macOS, a no-op on Windows (see
/// `platform::sync_parent_dir`).
///
/// Routed through one function so the unit tests can confirm that
/// the directory sync was requested (see `fence_probe`).
#[inline]
pub(crate) fn sync_parent(path: &std::path::Path) -> crate::Result<()> {
    #[cfg(test)]
    fence_probe::record_dir_sync();
    crate::platform::sync_parent_dir(path)
}

/// Test-only counters of [`fence_data`] and [`sync_parent`] calls made
/// on the current thread. Lets durability tests assert that a path
/// issued its fences without relying on timing or a real power cut.
#[cfg(test)]
pub(crate) mod fence_probe {
    use std::cell::Cell;

    thread_local! {
        static FENCES: Cell<u64> = const { Cell::new(0) };
        static DIR_SYNCS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn record_fence() {
        FENCES.with(|c| c.set(c.get().saturating_add(1)));
    }

    pub(crate) fn record_dir_sync() {
        DIR_SYNCS.with(|c| c.set(c.get().saturating_add(1)));
    }

    /// Number of data fences recorded on this thread so far.
    pub(crate) fn count() -> u64 {
        FENCES.with(Cell::get)
    }

    /// Number of parent-directory syncs requested on this thread.
    pub(crate) fn dir_syncs() -> u64 {
        DIR_SYNCS.with(Cell::get)
    }
}
