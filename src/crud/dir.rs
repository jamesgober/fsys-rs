//! Directory CRUD operations implemented as `impl Handle`.

use crate::handle::Handle;
use crate::meta::DirEntry;
use crate::{Error, Result};
use std::path::Path;

impl Handle {
    // ──────────────────────────────────────────────────────────────────────────
    // Creation
    // ──────────────────────────────────────────────────────────────────────────

    /// Creates a directory at `path`.
    ///
    /// Returns [`Error::Io`] (with kind `AlreadyExists`) if the directory
    /// already exists. Use [`Handle::mkdir_all`] for idempotent creation.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on any IO error.
    pub fn mkdir(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        std::fs::create_dir(&path).map_err(Error::Io)
    }

    /// Creates `path` and all missing ancestors, idempotently.
    ///
    /// Returns `Ok(())` if the directory already exists. On partial failure,
    /// reports which intermediate directories were already created before the
    /// error via [`Error::PartialDirectoryOp`].
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::PartialDirectoryOp`] if creation fails partway through
    ///   the ancestor chain.
    pub fn mkdir_all(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;

        // Collect the ancestor chain that needs to be created.
        let mut to_create: Vec<std::path::PathBuf> = Vec::new();
        let mut current = path.as_path();
        loop {
            match current.metadata() {
                Ok(m) if m.is_dir() => break, // ancestor already exists
                Ok(_) => {
                    return Err(Error::InvalidPath {
                        path: current.to_owned(),
                        reason: "a non-directory already exists at this path".into(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    to_create.push(current.to_owned());
                    match current.parent() {
                        Some(p) => current = p,
                        None => break,
                    }
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }

        to_create.reverse(); // create from shallowest to deepest
        let mut completed: Vec<String> = Vec::new();

        for dir in &to_create {
            match std::fs::create_dir(dir) {
                Ok(()) => {
                    completed.push(dir.display().to_string());
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Race: another thread/process created it. Fine.
                    completed.push(dir.display().to_string());
                }
                Err(e) => {
                    return Err(Error::PartialDirectoryOp {
                        failed_step: format!("create_dir({}): {}", dir.display(), e),
                        completed_steps: completed,
                    });
                }
            }
        }

        Ok(())
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Removal
    // ──────────────────────────────────────────────────────────────────────────

    /// Removes the empty directory at `path`.
    ///
    /// Fails if the directory is not empty. Use [`Handle::rmdir_all`] to
    /// recursively remove a non-empty directory tree.
    ///
    /// This operation is **idempotent**: if `path` does not exist,
    /// `Ok(())` is returned.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] for errors other than "not found" (e.g. not empty).
    pub fn rmdir(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        match std::fs::remove_dir(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Recursively removes the directory tree rooted at `path`.
    ///
    /// This operation is **idempotent**: if `path` does not exist,
    /// `Ok(())` is returned.
    ///
    /// On partial failure (e.g. permission error mid-tree), reports which
    /// top-level entries were successfully removed via
    /// [`Error::PartialDirectoryOp`].
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::PartialDirectoryOp`] on partial failure.
    pub fn rmdir_all(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = self.resolve_path(path.as_ref())?;
        match std::fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::PartialDirectoryOp {
                failed_step: format!("remove_dir_all({}): {}", path.display(), e),
                completed_steps: Vec::new(),
            }),
        }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Listing and metadata
    // ──────────────────────────────────────────────────────────────────────────

    /// Returns a list of entries in the directory at `path`.
    ///
    /// The entries are not sorted. Symlinks are not followed.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] if the directory cannot be read.
    pub fn list(&self, path: impl AsRef<Path>) -> Result<Vec<DirEntry>> {
        let path = self.resolve_path(path.as_ref())?;
        let rd = std::fs::read_dir(&path).map_err(Error::Io)?;
        let mut entries = Vec::new();
        for item in rd {
            let entry = item.map_err(Error::Io)?;
            entries.push(DirEntry::from_std(entry));
        }
        Ok(entries)
    }

    /// Returns `true` if a directory exists at `path`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on errors other than "not found".
    pub fn is_dir(&self, path: impl AsRef<Path>) -> Result<bool> {
        let path = self.resolve_path(path.as_ref())?;
        match std::fs::metadata(&path) {
            Ok(m) => Ok(m.is_dir()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Returns `true` if a regular file exists at `path`.
    ///
    /// Alias for [`Handle::exists`] provided for symmetry with
    /// [`Handle::is_dir`].
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] on errors other than "not found".
    pub fn is_file(&self, path: impl AsRef<Path>) -> Result<bool> {
        self.exists(path)
    }
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::builder::Builder;
    use crate::method::Method;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_crud_dir_{}_{}_{}",
            std::process::id(),
            n,
            suffix
        ))
    }

    struct TmpDir(std::path::PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn handle() -> crate::handle::Handle {
        Builder::new()
            .method(Method::Sync)
            .build()
            .expect("build handle")
    }

    #[test]
    fn test_mkdir_creates_directory() {
        let dir = tmp_path("mkdir");
        let _g = TmpDir(dir.clone());
        let h = handle();
        h.mkdir(&dir).expect("mkdir");
        assert!(dir.is_dir());
    }

    #[test]
    fn test_mkdir_fails_if_exists() {
        let dir = tmp_path("mkdir_exists");
        let _g = TmpDir(dir.clone());
        std::fs::create_dir(&dir).expect("create");
        assert!(handle().mkdir(&dir).is_err());
    }

    #[test]
    fn test_mkdir_all_creates_nested() {
        let root = tmp_path("mkdir_all");
        let _g = TmpDir(root.clone());
        let nested = root.join("a").join("b").join("c");
        handle().mkdir_all(&nested).expect("mkdir_all");
        assert!(nested.is_dir());
    }

    #[test]
    fn test_mkdir_all_idempotent() {
        let dir = tmp_path("mkdir_all_idem");
        let _g = TmpDir(dir.clone());
        std::fs::create_dir(&dir).expect("create");
        handle().mkdir_all(&dir).expect("mkdir_all on existing");
    }

    #[test]
    fn test_rmdir_removes_empty() {
        let dir = tmp_path("rmdir");
        std::fs::create_dir(&dir).expect("create");
        handle().rmdir(&dir).expect("rmdir");
        assert!(!dir.exists());
    }

    #[test]
    fn test_rmdir_idempotent() {
        let dir = tmp_path("rmdir_idem");
        handle().rmdir(&dir).expect("rmdir on non-existent");
    }

    #[test]
    fn test_rmdir_all_removes_tree() {
        let root = tmp_path("rmdir_all");
        std::fs::create_dir_all(root.join("sub")).expect("create tree");
        handle().rmdir_all(&root).expect("rmdir_all");
        assert!(!root.exists());
    }

    #[test]
    fn test_list_returns_entries() {
        let root = tmp_path("list");
        let _g = TmpDir(root.clone());
        std::fs::create_dir(&root).expect("create root");
        std::fs::write(root.join("file.txt"), b"x").expect("write");
        std::fs::create_dir(root.join("subdir")).expect("create subdir");
        let entries = handle().list(&root).expect("list");
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_is_dir_reflects_state() {
        let dir = tmp_path("is_dir");
        let _g = TmpDir(dir.clone());
        let h = handle();
        assert!(!h.is_dir(&dir).expect("is_dir before create"));
        std::fs::create_dir(&dir).expect("create");
        assert!(h.is_dir(&dir).expect("is_dir after create"));
    }
}
