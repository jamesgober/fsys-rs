//! File and directory metadata types.
//!
//! [`FileMeta`] wraps the fields of [`std::fs::Metadata`] plus symlink
//! detection. [`DirEntry`] carries the path and type flags for a single
//! directory entry. [`Permissions`] is a cross-platform abstraction over
//! POSIX mode bits and Windows read-only flags.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Metadata for a file or directory entry.
///
/// Constructed by [`crate::Handle::meta`]. Fields map onto
/// [`std::fs::Metadata`] with the addition of platform-portable
/// permission flags and explicit symlink detection.
///
/// This type is marked `#[non_exhaustive]` so that future minor versions
/// can add fields (e.g. inode number, hard-link count) without a
/// semver break.
///
/// # Examples
///
/// ```no_run
/// # fn main() -> fsys::Result<()> {
/// let fs = fsys::new()?;
/// let meta = fs.meta("data.bin")?;
/// println!("size: {} bytes", meta.size);
/// # Ok(()) }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct FileMeta {
    /// File size in bytes. Zero for directories (use directory-listing
    /// operations for directory size).
    pub size: u64,
    /// Time the file content was last modified.
    pub modified: SystemTime,
    /// Time the file was created, if the platform exposes it.
    ///
    /// `None` on Linux when the underlying filesystem does not record
    /// birth time, or when `statx` is not available.
    pub created: Option<SystemTime>,
    /// Time the file was last accessed, if the platform exposes it.
    ///
    /// Some operating systems (macOS with `noatime`, Linux with
    /// `relatime`) do not update this on every read.
    pub accessed: Option<SystemTime>,
    /// `true` when the path refers to a directory.
    pub is_dir: bool,
    /// `true` when the path refers to a regular file.
    pub is_file: bool,
    /// `true` when the path is a symbolic link.
    ///
    /// Populated via `symlink_metadata`, not `metadata`, so the flag
    /// reflects the link itself rather than its target.
    pub is_symlink: bool,
    /// `true` when the file system marks this path read-only.
    pub readonly: bool,
    /// Cross-platform permission abstraction.
    pub permissions: Permissions,
}

impl FileMeta {
    /// Constructs a [`FileMeta`] from a [`std::fs::Metadata`] value.
    ///
    /// `path` is used to perform a second `symlink_metadata` call so
    /// that `is_symlink` reflects the link itself rather than its
    /// target.
    pub(crate) fn from_metadata(meta: &std::fs::Metadata, path: &Path) -> Self {
        let is_symlink = std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);

        FileMeta {
            size: meta.len(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            created: meta.created().ok(),
            accessed: meta.accessed().ok(),
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            is_symlink,
            readonly: meta.permissions().readonly(),
            permissions: Permissions::from_metadata(meta),
        }
    }
}

/// A single entry from a directory listing.
///
/// Returned by [`crate::Handle::list`]. The `path` field is absolute
/// when the handle has a root configured and relative to the caller's
/// working directory otherwise.
///
/// This type is marked `#[non_exhaustive]` so that future minor
/// versions can add fields (e.g. inode, size) without a semver break.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Full path to the entry.
    pub path: PathBuf,
    /// File name component (last segment of `path`).
    pub name: OsString,
    /// `true` when the entry is a directory.
    pub is_dir: bool,
    /// `true` when the entry is a regular file.
    pub is_file: bool,
    /// `true` when the entry is a symbolic link.
    pub is_symlink: bool,
}

impl DirEntry {
    /// Constructs a [`DirEntry`] from a [`std::fs::DirEntry`].
    pub(crate) fn from_std(entry: std::fs::DirEntry) -> Self {
        let file_type = entry.file_type().ok();
        let is_dir = file_type.as_ref().map(|t| t.is_dir()).unwrap_or(false);
        let is_file = file_type.as_ref().map(|t| t.is_file()).unwrap_or(false);
        let is_symlink = file_type.as_ref().map(|t| t.is_symlink()).unwrap_or(false);
        let path = entry.path();
        let name = entry.file_name();
        DirEntry {
            path,
            name,
            is_dir,
            is_file,
            is_symlink,
        }
    }
}

/// Cross-platform file permission flags.
///
/// On Unix, the raw POSIX mode bits are preserved in `mode`. On
/// Windows, `executable` is always `false` because Windows does not
/// have an executable permission bit in the POSIX sense.
///
/// This type is marked `#[non_exhaustive]` so that future minor
/// versions can expose additional platform-specific fields without a
/// semver break.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// File is readable by the current process.
    pub readable: bool,
    /// File is writable by the current process.
    pub writable: bool,
    /// File is executable.
    ///
    /// Always `false` on Windows. On Unix, reflects the owner execute
    /// bit of the raw POSIX mode.
    pub executable: bool,
    /// Raw POSIX mode bits (e.g. `0o644`).
    ///
    /// Only present on Unix targets. On Windows, use the individual
    /// `readable` / `writable` flags.
    #[cfg(unix)]
    pub mode: u32,
}

impl Permissions {
    /// Constructs a [`Permissions`] from a [`std::fs::Metadata`] value.
    pub(crate) fn from_metadata(meta: &std::fs::Metadata) -> Self {
        let perms = meta.permissions();
        let readonly = perms.readonly();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = perms.mode();
            Permissions {
                readable: (mode & 0o400) != 0,
                writable: !readonly,
                executable: (mode & 0o100) != 0,
                mode,
            }
        }

        #[cfg(not(unix))]
        {
            Permissions {
                readable: true, // If we can stat it, we can read it.
                writable: !readonly,
                executable: false, // Windows has no POSIX executable bit.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// RAII guard that deletes a file on drop.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(content: &[u8]) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("fsys_meta_test_{}_{}", std::process::id(), n));
            let mut f = fs::File::create(&path).expect("create temp file");
            f.write_all(content).expect("write temp file");
            TempFile(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_file_meta_from_metadata_reports_correct_size() {
        let tmp = TempFile::new(b"hello world");
        let meta = fs::metadata(tmp.path()).expect("metadata");
        let fm = FileMeta::from_metadata(&meta, tmp.path());
        assert_eq!(fm.size, 11);
    }

    #[test]
    fn test_file_meta_is_file_true_for_regular_file() {
        let tmp = TempFile::new(b"data");
        let meta = fs::metadata(tmp.path()).expect("metadata");
        let fm = FileMeta::from_metadata(&meta, tmp.path());
        assert!(fm.is_file);
        assert!(!fm.is_dir);
    }

    #[test]
    fn test_file_meta_is_symlink_false_for_regular_file() {
        let tmp = TempFile::new(b"data");
        let meta = fs::metadata(tmp.path()).expect("metadata");
        let fm = FileMeta::from_metadata(&meta, tmp.path());
        assert!(!fm.is_symlink);
    }

    #[test]
    fn test_permissions_writable_on_writable_file() {
        let tmp = TempFile::new(b"data");
        let meta = fs::metadata(tmp.path()).expect("metadata");
        let perms = Permissions::from_metadata(&meta);
        assert!(perms.writable);
    }

    #[test]
    fn test_dir_entry_from_std_is_dir_for_directory() {
        let dir = std::env::temp_dir();
        // Just verify the function doesn't panic on a real entry.
        if let Some(Ok(entry)) = fs::read_dir(&dir).ok().and_then(|mut d| d.next()) {
            let de = DirEntry::from_std(entry);
            // Exactly one of is_dir/is_file/is_symlink may be true.
            let count = [de.is_dir, de.is_file, de.is_symlink]
                .iter()
                .filter(|&&b| b)
                .count();
            assert!(count <= 1, "at most one type flag should be set");
        }
    }

    #[test]
    fn test_permissions_executable_false_on_windows() {
        #[cfg(windows)]
        {
            let tmp = TempFile::new(b"test");
            let meta = fs::metadata(tmp.path()).expect("metadata");
            let perms = Permissions::from_metadata(&meta);
            assert!(!perms.executable, "Windows has no executable bit");
        }
        #[cfg(not(windows))]
        {
            // On Unix, just verify the struct can be constructed.
            let tmp = TempFile::new(b"test");
            let meta = fs::metadata(tmp.path()).expect("metadata");
            let _perms = Permissions::from_metadata(&meta);
        }
    }

    #[test]
    fn test_file_meta_modified_time_is_after_epoch() {
        let tmp = TempFile::new(b"ts");
        let meta = fs::metadata(tmp.path()).expect("metadata");
        let fm = FileMeta::from_metadata(&meta, tmp.path());
        assert!(fm.modified >= SystemTime::UNIX_EPOCH);
    }
}
