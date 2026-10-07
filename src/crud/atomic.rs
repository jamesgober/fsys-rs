//! The atomic-replace sequence shared by every whole-file write path.
//!
//! [`crate::Handle::write`], [`crate::Handle::write_copy`] and the
//! group-lane batch executor all publish new file contents the same
//! way:
//!
//! 1. Create a fresh temp file next to the target.
//! 2. Write the payload (Direct IO or buffered).
//! 3. Make the temp file durable: the Direct path trims padding and
//!    fences, the buffered path flushes with the method's primitive.
//! 4. Run any pre-rename step (metadata copy for `write_copy`).
//! 5. `rename(temp, target)`, atomic within one filesystem.
//! 6. Sync the parent directory (unless the caller batches that).
//!
//! The temp file is removed on every failure path, including a panic
//! raised by a hook, through [`TempGuard`].
//!
//! Callers customise the sequence through [`ReplaceHooks`] rather than
//! copying it, so a durability fix lands in one place. The trait is
//! synchronous; an async caller can reuse [`ReplacePlan`] and the
//! hook shape around its own write step.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::handle::Handle;
use crate::method::Method;
use crate::platform;
use crate::{Error, Result};

/// Outcome of a multi-step durable write: `Err` carries the name of
/// the atomic-replace step that failed (reported through
/// [`Error::AtomicReplaceFailed::step`]) and the underlying error.
pub(crate) type StepResult = std::result::Result<(), (&'static str, Error)>;

/// Flush primitive for a temp file written through the buffered path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BufferedFlush {
    /// Data-level fence (`fdatasync` / `F_FULLFSYNC` /
    /// `FlushFileBuffers`).
    Data,
    /// Full `fsync` (data and all metadata).
    Full,
}

impl BufferedFlush {
    /// Picks the buffered flush for `method`.
    ///
    /// `Direct` maps to the data fence: a Direct request only reaches
    /// the buffered path when the filesystem refused Direct IO for the
    /// temp file, and the buffered bytes then need a real flush.
    pub(crate) fn for_method(method: Method) -> Self {
        match method {
            Method::Data | Method::Direct => BufferedFlush::Data,
            _ => BufferedFlush::Full,
        }
    }

    fn run(self, file: &File) -> Result<()> {
        match self {
            BufferedFlush::Data => super::fence_data(file),
            BufferedFlush::Full => platform::sync_full(file),
        }
    }
}

/// Static parameters of one atomic replace.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplacePlan {
    /// Request Direct IO for the temp file. The filesystem may refuse
    /// it, in which case the buffered path runs.
    pub use_direct: bool,
    /// Flush used when the temp file was written through the buffered
    /// path.
    pub flush: BufferedFlush,
    /// Sync the parent directory after the rename. The group lane's
    /// grouped commit turns this off and syncs each directory once.
    pub sync_parent: bool,
}

/// Per-caller steps plugged into [`atomic_replace`].
pub(crate) trait ReplaceHooks {
    /// Called when Direct IO was requested but the filesystem opened
    /// the temp file without it.
    fn direct_refused(&self) {}

    /// Writes `data` to the freshly created Direct-IO temp `file` and
    /// leaves it durable at exactly `data.len()` bytes.
    fn write_direct_durable(&self, file: &File, data: &[u8]) -> StepResult;

    /// Runs after the temp file is durable and closed, immediately
    /// before the rename.
    fn before_rename(&self, _temp: &Path) {}
}

/// Hooks for callers without handle state (the group lane): Direct
/// writes go through the platform `pwrite` / `WriteFile` path.
pub(crate) struct PlatformHooks {
    /// Logical sector size used to pad Direct writes.
    pub sector_size: u32,
}

impl ReplaceHooks for PlatformHooks {
    fn write_direct_durable(&self, file: &File, data: &[u8]) -> StepResult {
        super::file::write_direct_durable_platform(file, data, self.sector_size)
    }
}

/// Removes the temp file when dropped while armed.
///
/// The temp `File` itself is owned by [`write_temp`], whose frame
/// unwinds before the guard drops, so the handle is always closed
/// before the removal (an open file without `FILE_SHARE_DELETE` cannot
/// be removed on Windows).
struct TempGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl Drop for TempGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Cleanup of a temp file that never became visible at the
            // target path. The primary error (or panic) is already on
            // its way to the caller; a failed removal only leaves an
            // orphaned `.fsys-tmp-*` file behind.
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Runs the atomic-replace sequence described in the module docs.
///
/// # Errors
///
/// [`Error::AtomicReplaceFailed`] naming the failed step
/// (`open_temp`, `write`, `truncate`, `flush`, or `rename`). The target
/// is untouched for every failure before `rename`.
pub(crate) fn atomic_replace<H: ReplaceHooks>(
    target: &Path,
    data: &[u8],
    plan: &ReplacePlan,
    hooks: &H,
) -> Result<()> {
    let (temp, (file, direct_ok)) = with_unique_temp(target, |temp| {
        platform::open_write_new(temp, plan.use_direct)
    })
    .map_err(|e| step_err("open_temp", e))?;
    let mut guard = TempGuard {
        path: &temp,
        armed: true,
    };

    write_temp(file, direct_ok, data, plan, hooks).map_err(|(step, e)| step_err(step, e))?;

    hooks.before_rename(&temp);

    platform::atomic_rename(&temp, target).map_err(|e| step_err("rename", e))?;
    guard.armed = false;

    if plan.sync_parent {
        // Best-effort: the rename already published the new contents,
        // so reporting `AtomicReplaceFailed` here would wrongly tell
        // the caller the replace did not happen.
        let _ = platform::sync_parent_dir(target);
    }
    Ok(())
}

/// Writes and fences the temp file, then closes it (`file` is consumed
/// and dropped when this function returns or unwinds).
fn write_temp<H: ReplaceHooks>(
    file: File,
    direct_ok: bool,
    data: &[u8],
    plan: &ReplacePlan,
    hooks: &H,
) -> StepResult {
    if plan.use_direct && !direct_ok {
        hooks.direct_refused();
    }
    if direct_ok {
        hooks.write_direct_durable(&file, data)
    } else {
        platform::write_all(&file, data).map_err(|e| ("write", e))?;
        plan.flush.run(&file).map_err(|e| ("flush", e))
    }
}

/// Maximum number of temp names tried before giving up on
/// `AlreadyExists`. Names carry a pid and a 64-bit nonce, so more than
/// one retry indicates something other than chance (for example a
/// directory flooded with fsys-shaped names).
const TEMP_CREATE_ATTEMPTS: usize = 8;

/// Creates a temp file next to `target` through `open`, which must
/// perform an exclusive create (`O_EXCL` / `CREATE_NEW`). Retries with
/// a fresh name when the chosen name already exists.
///
/// # Errors
///
/// The last error from `open`: any non-`AlreadyExists` error at once,
/// or `AlreadyExists` after [`TEMP_CREATE_ATTEMPTS`] attempts.
pub(crate) fn with_unique_temp<T>(
    target: &Path,
    mut open: impl FnMut(&Path) -> Result<T>,
) -> Result<(PathBuf, T)> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let temp = Handle::gen_temp_path(target);
        match open(&temp) {
            Ok(v) => return Ok((temp, v)),
            Err(Error::Io(e))
                if e.kind() == std::io::ErrorKind::AlreadyExists
                    && attempt < TEMP_CREATE_ATTEMPTS => {}
            Err(e) => return Err(e),
        }
    }
}

fn step_err(step: &'static str, e: Error) -> Error {
    Error::AtomicReplaceFailed {
        step,
        source: as_io_error(e),
    }
}

/// Converts a `crate::Error` into the `std::io::Error` carried by
/// [`Error::AtomicReplaceFailed`].
pub(crate) fn as_io_error(e: Error) -> std::io::Error {
    match e {
        Error::Io(io_err) => io_err,
        other => std::io::Error::other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir(suffix: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fsys_atomic_{}_{}_{}",
            std::process::id(),
            n,
            suffix
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn plan() -> ReplacePlan {
        ReplacePlan {
            use_direct: false,
            flush: BufferedFlush::Full,
            sync_parent: true,
        }
    }

    #[test]
    fn test_buffered_flush_for_method_maps_direct_to_data_fence() {
        assert_eq!(
            BufferedFlush::for_method(Method::Direct),
            BufferedFlush::Data
        );
        assert_eq!(BufferedFlush::for_method(Method::Data), BufferedFlush::Data);
        assert_eq!(BufferedFlush::for_method(Method::Sync), BufferedFlush::Full);
        assert_eq!(BufferedFlush::for_method(Method::Mmap), BufferedFlush::Full);
    }

    #[test]
    fn test_buffered_flush_direct_on_buffered_file_flushes() {
        // A Direct request whose open fell back to buffered IO (e.g.
        // NO_BUFFERING rejected on Windows) leaves dirty pages behind;
        // the flush must be real on every platform.
        let dir = tmp_dir("flush_direct_fallback");
        let _g = DirGuard(dir.clone());
        let f = File::create(dir.join("f")).expect("create");
        let before = crate::crud::fence_probe::count();
        BufferedFlush::for_method(Method::Direct)
            .run(&f)
            .expect("direct fallback flush");
        assert_eq!(crate::crud::fence_probe::count(), before + 1);
    }

    #[test]
    fn test_buffered_flush_full_and_data_succeed() {
        let dir = tmp_dir("flush_kinds");
        let _g = DirGuard(dir.clone());
        let f = File::create(dir.join("f")).expect("create");
        BufferedFlush::Full.run(&f).expect("full flush");
        BufferedFlush::Data.run(&f).expect("data flush");
    }

    #[test]
    fn test_as_io_error_passes_through_io_variant() {
        let inner = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let io = as_io_error(Error::Io(inner));
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn test_as_io_error_wraps_non_io_variant() {
        let err = Error::HardwareProbeFailed {
            detail: "stub".into(),
        };
        // The display string of the original error is embedded.
        assert!(as_io_error(err).to_string().contains("FS-00003"));
    }

    #[test]
    fn test_with_unique_temp_retries_on_already_exists() {
        let target = Path::new("/nonexistent-dir/target.bin");
        let mut calls = 0;
        let (temp, value) = with_unique_temp(target, |_| {
            calls += 1;
            if calls < 3 {
                Err(Error::Io(std::io::ErrorKind::AlreadyExists.into()))
            } else {
                Ok(42)
            }
        })
        .expect("third attempt succeeds");
        assert_eq!(value, 42);
        assert_eq!(calls, 3);
        assert_eq!(temp.parent(), target.parent());
    }

    #[test]
    fn test_with_unique_temp_gives_up_after_bounded_attempts() {
        let mut calls = 0;
        let err = with_unique_temp(Path::new("t.bin"), |_| -> Result<()> {
            calls += 1;
            Err(Error::Io(std::io::ErrorKind::AlreadyExists.into()))
        })
        .expect_err("persistent collisions must surface");
        assert_eq!(calls, TEMP_CREATE_ATTEMPTS);
        assert!(matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists));
    }

    #[test]
    fn test_with_unique_temp_does_not_retry_other_errors() {
        let mut calls = 0;
        let _ = with_unique_temp(Path::new("t.bin"), |_| -> Result<()> {
            calls += 1;
            Err(Error::Io(std::io::ErrorKind::PermissionDenied.into()))
        })
        .expect_err("permission errors are not retried");
        assert_eq!(calls, 1);
    }

    #[test]
    fn test_atomic_replace_succeeds_next_to_orphaned_pre_1_1_1_temp_files() {
        // A crash in an earlier process leaves `.fsys-tmp-<n>.<name>`
        // behind. Every later write must still succeed.
        let dir = tmp_dir("orphans");
        let _g = DirGuard(dir.clone());
        let target = dir.join("t.bin");
        for n in 0..32 {
            std::fs::write(dir.join(format!(".fsys-tmp-{n}.t.bin")), b"orphan").expect("seed");
        }
        atomic_replace(
            &target,
            b"fresh",
            &plan(),
            &PlatformHooks { sector_size: 512 },
        )
        .expect("write next to orphans");
        assert_eq!(std::fs::read(&target).expect("read"), b"fresh");
    }

    #[cfg(unix)]
    #[test]
    fn test_atomic_replace_handles_maximum_length_file_names() {
        // A 255-byte target name is legal; the temp name must not push
        // past the component limit (ENAMETOOLONG before 1.1.1).
        let dir = tmp_dir("long_name");
        let _g = DirGuard(dir.clone());
        let target = dir.join("x".repeat(255));
        atomic_replace(
            &target,
            b"long",
            &plan(),
            &PlatformHooks { sector_size: 512 },
        )
        .expect("write to a 255-byte name");
        assert_eq!(std::fs::read(&target).expect("read"), b"long");
        assert_eq!(entries(&dir).len(), 1, "temp file leaked");
    }

    #[test]
    fn test_atomic_replace_writes_payload_and_leaves_no_temp() {
        let dir = tmp_dir("ok");
        let _g = DirGuard(dir.clone());
        let target = dir.join("t.bin");
        atomic_replace(
            &target,
            b"payload",
            &plan(),
            &PlatformHooks { sector_size: 512 },
        )
        .expect("replace");
        assert_eq!(std::fs::read(&target).expect("read"), b"payload");
        assert_eq!(entries(&dir), vec!["t.bin".to_string()]);
    }

    struct PanickingHooks;
    impl ReplaceHooks for PanickingHooks {
        fn write_direct_durable(&self, _file: &File, _data: &[u8]) -> StepResult {
            Ok(())
        }
        fn before_rename(&self, _temp: &Path) {
            panic!("hook panic for cleanup test");
        }
    }

    #[test]
    fn test_atomic_replace_removes_temp_when_a_hook_panics() {
        let dir = tmp_dir("panic");
        let _g = DirGuard(dir.clone());
        let target = dir.join("t.bin");
        std::fs::write(&target, b"old").expect("seed");
        let result =
            std::panic::catch_unwind(|| atomic_replace(&target, b"new", &plan(), &PanickingHooks));
        assert!(result.is_err(), "hook panic must propagate");
        assert_eq!(std::fs::read(&target).expect("read"), b"old");
        assert_eq!(entries(&dir), vec!["t.bin".to_string()], "temp file leaked");
    }

    #[test]
    fn test_atomic_replace_rename_failure_removes_temp() {
        let dir = tmp_dir("rename_fail");
        let _g = DirGuard(dir.clone());
        // A non-empty directory at the target makes the rename fail on
        // every platform.
        let target = dir.join("occupied");
        std::fs::create_dir(&target).expect("mkdir target");
        std::fs::write(target.join("inner"), b"x").expect("seed inner");
        let err = atomic_replace(
            &target,
            b"new",
            &plan(),
            &PlatformHooks { sector_size: 512 },
        )
        .expect_err("rename onto a non-empty directory must fail");
        assert!(matches!(
            err,
            Error::AtomicReplaceFailed { step: "rename", .. }
        ));
        assert_eq!(
            entries(&dir),
            vec!["occupied".to_string()],
            "temp file leaked"
        );
    }
}
