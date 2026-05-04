//! Convenience free functions for one-shot file IO.
//!
//! These functions use a lazily-initialised default [`Handle`] configured
//! with [`Method::Auto`] and no root restriction. They are the fastest way
//! to get data on disk without constructing a `Handle` explicitly.
//!
//! For performance-sensitive code, prefer a long-lived `Handle` obtained
//! from [`crate::builder()`] to avoid repeatedly probing hardware and
//! allocating state.
//!
//! # Example
//!
//! ```no_run
//! # fn example() -> fsys::Result<()> {
//! fsys::quick::write("/tmp/greeting.txt", b"hello")?;
//! let data = fsys::quick::read("/tmp/greeting.txt")?;
//! assert_eq!(data, b"hello");
//! # Ok(())
//! # }
//! ```

use crate::builder::Builder;
use crate::handle::Handle;
use crate::method::Method;
use crate::{Error, Result};
use std::path::Path;
use std::sync::OnceLock;

// ──────────────────────────────────────────────────────────────────────────────
// Default handle
// ──────────────────────────────────────────────────────────────────────────────

static DEFAULT_HANDLE: OnceLock<Handle> = OnceLock::new();

fn default_handle() -> Result<&'static Handle> {
    if let Some(h) = DEFAULT_HANDLE.get() {
        return Ok(h);
    }
    let h = Builder::new().method(Method::Auto).build()?;
    // Ignore a potential race; the losing thread's handle is simply dropped.
    let _ = DEFAULT_HANDLE.set(h);
    DEFAULT_HANDLE
        .get()
        .ok_or_else(|| Error::Io(std::io::Error::other("default handle init race")))
}

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/// Atomically writes `data` to `path`, replacing any existing file.
///
/// Uses the lazily-initialised default handle (Method::Auto). The first
/// call probes the hardware to select the optimal durability method.
///
/// # Errors
///
/// - [`Error::Io`] if the write fails.
/// - The first call can also return errors from hardware probing, but
///   these are non-fatal (the method falls back to `Sync`).
pub fn write(path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
    default_handle()?.write(path, data)
}

/// Atomically writes `data` to `path` using a transient handle configured
/// with `method`.
///
/// Unlike [`write()`], this function always constructs a fresh `Handle` (and
/// therefore re-probes the sector size). Use it when you need a one-off
/// write with a specific method and do not want to affect the shared
/// default handle.
///
/// # Errors
///
/// - [`Error::UnsupportedMethod`] for reserved method variants.
/// - [`Error::Io`] if the write fails.
pub fn write_with(path: impl AsRef<Path>, data: &[u8], method: Method) -> Result<()> {
    Builder::new().method(method).build()?.write(path, data)
}

/// Reads the entire contents of `path`.
///
/// # Errors
///
/// - [`Error::Io`] if the file does not exist or cannot be read.
pub fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    default_handle()?.read(path)
}

/// Deletes the file at `path` (idempotent).
///
/// Returns `Ok(())` if the file does not exist.
///
/// # Errors
///
/// - [`Error::Io`] for errors other than "not found".
pub fn delete(path: impl AsRef<Path>) -> Result<()> {
    default_handle()?.delete(path)
}

/// Returns `true` if a regular file exists at `path`.
///
/// # Errors
///
/// - [`Error::Io`] for errors other than "not found".
pub fn exists(path: impl AsRef<Path>) -> Result<bool> {
    default_handle()?.exists(path)
}

/// Returns the size of the file at `path` in bytes.
///
/// # Errors
///
/// - [`Error::Io`] if the file does not exist or cannot be accessed.
pub fn size(path: impl AsRef<Path>) -> Result<u64> {
    default_handle()?.size(path)
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_quick_{}_{}_{}",
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

    #[test]
    fn test_quick_write_and_read() {
        let path = tmp_path("rw");
        let _g = TmpFile(path.clone());
        write(&path, b"quick").expect("write");
        let data = read(&path).expect("read");
        assert_eq!(data, b"quick");
    }

    #[test]
    fn test_quick_exists() {
        let path = tmp_path("exists");
        let _g = TmpFile(path.clone());
        assert!(!exists(&path).expect("exists before"));
        write(&path, b"x").expect("write");
        assert!(exists(&path).expect("exists after"));
    }

    #[test]
    fn test_quick_delete_idempotent() {
        let path = tmp_path("delete");
        write(&path, b"y").expect("write");
        delete(&path).expect("delete");
        delete(&path).expect("delete again");
    }

    #[test]
    fn test_quick_size() {
        let path = tmp_path("size");
        let _g = TmpFile(path.clone());
        write(&path, b"hello").expect("write");
        assert_eq!(size(&path).expect("size"), 5);
    }

    #[test]
    fn test_write_with_sync_method() {
        let path = tmp_path("write_with");
        let _g = TmpFile(path.clone());
        write_with(&path, b"sync write", Method::Sync).expect("write_with");
        assert_eq!(std::fs::read(&path).expect("read"), b"sync write");
    }
}
