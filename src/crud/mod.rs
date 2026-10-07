//! CRUD operations on files and directories.
//!
//! All operations are implemented as `impl Handle` blocks:
//! - `file`: file write, read, append, delete, copy, exists, size, metadata.
//! - `dir`: directory create, remove, list, exists.

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
    fence_probe::record();
    crate::platform::sync_data(file)
}

/// Test-only counter of [`fence_data`] calls made on the current
/// thread. Lets durability tests assert that a write path fenced its
/// data without relying on timing or on a real power cut.
#[cfg(test)]
pub(crate) mod fence_probe {
    use std::cell::Cell;

    thread_local! {
        static FENCES: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn record() {
        FENCES.with(|c| c.set(c.get().saturating_add(1)));
    }

    /// Number of fences recorded on this thread so far.
    pub(crate) fn count() -> u64 {
        FENCES.with(Cell::get)
    }
}
