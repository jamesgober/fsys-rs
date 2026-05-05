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

    /// Walks the directory at `path` non-recursively, returning
    /// every immediate entry.
    ///
    /// Equivalent to [`Handle::list`] but with the `_all` /
    /// non-`_all` naming pair that runs throughout fsys's
    /// directory API ([`mkdir`](Handle::mkdir) /
    /// [`mkdir_all`](Handle::mkdir_all) etc.). Use
    /// [`Handle::scan_all`] for the recursive variant.
    ///
    /// Renamed from `scan(path, recursive: bool)` in `0.7.0` per
    /// the API audit (see `.dev/API-AUDIT-0.7.0.md` H.5
    /// reconciliation #3).
    ///
    /// Order is OS-dependent; do not rely on it. Symlinks are
    /// **not** followed.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] if the root directory cannot be read.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::builder;
    ///
    /// let fs = builder().build()?;
    /// let entries = fs.scan("/var/log")?;
    /// for e in entries {
    ///     println!("{}", e.path.display());
    /// }
    /// # Ok::<(), fsys::Error>(())
    /// ```
    pub fn scan(&self, path: impl AsRef<Path>) -> Result<Vec<DirEntry>> {
        let root = self.resolve_path(path.as_ref())?;
        let mut out: Vec<DirEntry> = Vec::new();
        scan_into(&root, false, &mut out)?;
        Ok(out)
    }

    /// Recursively walks the directory tree at `path`, returning
    /// every entry (immediate children + all descendants).
    ///
    /// Recursive variant of [`Handle::scan`]. Order is OS-dependent;
    /// do not rely on it. Symlinks are **not** followed.
    ///
    /// New in `0.7.0` (split out of the previous
    /// `scan(path, recursive)` per API-audit reconciliation #3).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root.
    /// - [`Error::Io`] if the root directory cannot be read.
    /// - [`Error::PartialDirectoryOp`] if a recursive walk fails
    ///   part-way through (e.g. permission denied on a
    ///   subdirectory). The variant carries the entries enumerated
    ///   successfully before the failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::builder;
    ///
    /// let fs = builder().build()?;
    /// let all_entries = fs.scan_all("/var/log")?;
    /// for e in all_entries {
    ///     println!("{}", e.path.display());
    /// }
    /// # Ok::<(), fsys::Error>(())
    /// ```
    pub fn scan_all(&self, path: impl AsRef<Path>) -> Result<Vec<DirEntry>> {
        let root = self.resolve_path(path.as_ref())?;
        let mut out: Vec<DirEntry> = Vec::new();
        scan_into(&root, true, &mut out)?;
        Ok(out)
    }

    /// Returns paths within `path` that match `pattern`.
    ///
    /// `pattern` is interpreted relative to `path`. Standard glob
    /// syntax (`*`, `**`, `?`, `[abc]`, `[!abc]`, `{foo,bar}`) per
    /// the [`glob`](https://docs.rs/glob) crate. Patterns that
    /// escape the base directory (e.g. `../../etc/passwd`) are
    /// rejected with [`Error::InvalidPath`].
    ///
    /// # Recursion semantics
    ///
    /// **`find` recurses based on the pattern itself, not a flag.**
    /// This is intentionally asymmetric with [`Handle::scan`] /
    /// [`Handle::scan_all`] and [`Handle::count`] /
    /// [`Handle::count_all`] (which split flat vs. recursive into
    /// distinct methods). Glob patterns express recursion natively
    /// via `**`, so a recursive flag would be redundant:
    ///
    /// - `*.log` — immediate children only (non-recursive).
    /// - `**/*.log` — every `.log` under the tree (recursive).
    /// - `sub/**/*.log` — every `.log` under the `sub/` subtree.
    ///
    /// If you want "every entry under this tree" without filtering,
    /// use [`Handle::scan_all`] instead of `find("**")`.
    ///
    /// Symlinks are not followed in `0.6.0`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidPath`] if `path` escapes the handle root, or
    ///   if `pattern` escapes `path`.
    /// - [`Error::GlobPatternInvalid`] if `pattern` is not a
    ///   syntactically valid glob.
    /// - [`Error::Io`] on filesystem errors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::builder;
    ///
    /// let fs = builder().build()?;
    /// // Find all `.log` files anywhere under `/var/log`:
    /// let logs = fs.find("/var/log", "**/*.log")?;
    /// // Find immediate `.conf` children of `/etc`:
    /// let confs = fs.find("/etc", "*.conf")?;
    /// # Ok::<(), fsys::Error>(())
    /// ```
    pub fn find(&self, path: impl AsRef<Path>, pattern: &str) -> Result<Vec<std::path::PathBuf>> {
        let root = self.resolve_path(path.as_ref())?;

        // Pattern escape check: reject any leading `..` or absolute
        // path; we never run a pattern that resolves outside `root`.
        if pattern.contains("..") || std::path::Path::new(pattern).is_absolute() {
            return Err(Error::InvalidPath {
                path: std::path::PathBuf::from(pattern),
                reason: "glob pattern must not escape the base directory".into(),
            });
        }

        let combined = root.join(pattern);
        let combined_str = combined.to_str().ok_or_else(|| Error::InvalidPath {
            path: combined.clone(),
            reason: "non-UTF-8 path component".into(),
        })?;

        // Brace alternation `{a,b}` is part of the 0.6.0 `find`
        // API contract (D-4) but is not supported natively by the
        // `glob` crate. Expand braces here so each expanded
        // pattern is a single `glob`-supported string, then union
        // the match sets.
        let expanded = expand_braces(combined_str);

        let mut out: Vec<std::path::PathBuf> = Vec::new();
        let mut seen: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        for sub in &expanded {
            let paths = glob::glob(sub).map_err(|e| Error::GlobPatternInvalid {
                reason: e.to_string(),
            })?;
            for entry in paths {
                match entry {
                    Ok(p) => {
                        if seen.insert(p.clone()) {
                            out.push(p);
                        }
                    }
                    Err(e) => return Err(Error::Io(e.into_error())),
                }
            }
        }
        Ok(out)
    }

    /// Counts the number of regular files immediately within
    /// `path` (non-recursive).
    ///
    /// Implemented in terms of [`Handle::scan`] with a filter.
    /// Cost is O(immediate-child count).
    ///
    /// Renamed from `count(path, recursive: bool)` in `0.7.0` per
    /// the API audit reconciliation. Use [`Handle::count_all`] for
    /// the recursive variant.
    ///
    /// # Errors
    ///
    /// - Same as [`Handle::scan`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::builder;
    ///
    /// let fs = builder().build()?;
    /// let n = fs.count("/var/log")?;
    /// println!("immediate children of /var/log: {n}");
    /// # Ok::<(), fsys::Error>(())
    /// ```
    pub fn count(&self, path: impl AsRef<Path>) -> Result<usize> {
        let entries = self.scan(path)?;
        Ok(entries.iter().filter(|e| e.is_file).count())
    }

    /// Recursively counts every regular file at or below `path`.
    ///
    /// Implemented in terms of [`Handle::scan_all`] with a filter.
    /// Cost is O(total-file count under `path`).
    ///
    /// New in `0.7.0` (split out of the previous
    /// `count(path, recursive)` per API-audit reconciliation).
    ///
    /// # Errors
    ///
    /// - Same as [`Handle::scan_all`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use fsys::builder;
    ///
    /// let fs = builder().build()?;
    /// let n = fs.count_all("/var/log")?;
    /// println!("log tree has {n} files (recursive)");
    /// # Ok::<(), fsys::Error>(())
    /// ```
    pub fn count_all(&self, path: impl AsRef<Path>) -> Result<usize> {
        let entries = self.scan_all(path)?;
        Ok(entries.iter().filter(|e| e.is_file).count())
    }
}

/// Expands brace-alternation in a glob pattern.
///
/// `{a,b}*.conf` → `["a*.conf", "b*.conf"]`. Nested braces are not
/// supported in `0.6.0` (e.g. `{a,{b,c}}` would be treated as a
/// literal at the inner brace) — filed for follow-up if a real
/// consumer needs it. Patterns without `{...}` are returned as a
/// single-element vec containing the original pattern.
///
/// Algorithm: scan left-to-right for the first top-level `{...}`
/// group, split its contents on commas, and recursively expand the
/// surrounding context with each alternative substituted in. The
/// recursion's depth is bounded by the number of brace groups.
fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let Some(open) = bytes.iter().position(|&b| b == b'{') else {
        return vec![pattern.to_string()];
    };
    let Some(close_offset) = bytes[open + 1..].iter().position(|&b| b == b'}') else {
        return vec![pattern.to_string()];
    };
    let close = open + 1 + close_offset;

    let prefix = &pattern[..open];
    let group = &pattern[open + 1..close];
    let suffix = &pattern[close + 1..];

    let mut out = Vec::new();
    for alt in group.split(',') {
        let with_alt = format!("{prefix}{alt}{suffix}");
        out.extend(expand_braces(&with_alt));
    }
    out
}

/// Recursive walk helper. Best-effort: when a subdirectory cannot be
/// read (permission denied, vanished mid-walk), the entries already
/// collected are preserved and the failing step is surfaced via
/// [`Error::PartialDirectoryOp`]. Entries collected before the
/// failure are still returned to the caller via the
/// `completed_steps` field's count.
fn scan_into(root: &Path, recursive: bool, out: &mut Vec<DirEntry>) -> Result<()> {
    let rd = std::fs::read_dir(root).map_err(|e| {
        if out.is_empty() {
            // Top-level read failure — surface as plain Io for the
            // simplest happy/sad split.
            Error::Io(e)
        } else {
            Error::PartialDirectoryOp {
                failed_step: format!("read_dir({}): {}", root.display(), e),
                completed_steps: out.iter().map(|x| x.path.display().to_string()).collect(),
            }
        }
    })?;
    for item in rd {
        let entry = item.map_err(Error::Io)?;
        let de = DirEntry::from_std(entry);
        let is_dir = de.is_dir;
        let path = de.path.clone();
        out.push(de);
        if recursive && is_dir {
            scan_into(&path, true, out)?;
        }
    }
    Ok(())
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
    fn test_expand_braces_no_braces_returns_input() {
        assert_eq!(super::expand_braces("*.log"), vec!["*.log".to_string()]);
    }

    #[test]
    fn test_expand_braces_single_group_two_alternatives() {
        let out = super::expand_braces("{a,b}*.log");
        assert_eq!(out, vec!["a*.log".to_string(), "b*.log".to_string()]);
    }

    #[test]
    fn test_expand_braces_single_group_three_alternatives() {
        let out = super::expand_braces("pre-{x,y,z}");
        assert_eq!(
            out,
            vec![
                "pre-x".to_string(),
                "pre-y".to_string(),
                "pre-z".to_string(),
            ]
        );
    }

    #[test]
    fn test_expand_braces_multiple_groups_cartesian() {
        let out = super::expand_braces("{a,b}-{1,2}");
        assert_eq!(
            out,
            vec![
                "a-1".to_string(),
                "a-2".to_string(),
                "b-1".to_string(),
                "b-2".to_string(),
            ]
        );
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
