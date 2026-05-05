//! Error type for the `fsys` crate.
//!
//! All fallible operations in `fsys` return [`Result<T>`], which is a type
//! alias for [`std::result::Result<T, Error>`]. The [`Error`] enum is the
//! single error type produced by the library; consumers match on its
//! variants rather than juggling boxed trait objects.
//!
//! Error codes use the prefix `FS-` per the wider Hive error registry.
//! Codes are stable once assigned: the integer associated with each variant
//! is part of the public contract and will not change between versions.

use std::fmt;
use std::path::PathBuf;

/// Convenient `Result` alias that fixes the error type to [`Error`].
///
/// # Examples
///
/// ```
/// use fsys::Result;
///
/// fn always_ok() -> Result<u32> {
///     Ok(42)
/// }
///
/// assert_eq!(always_ok().ok(), Some(42));
/// ```
pub type Result<T> = std::result::Result<T, Error>;

/// The single error type produced by the `fsys` crate.
///
/// Every variant carries enough context to identify what was attempted,
/// where it failed, and what the caller can do next. Display output never
/// includes raw buffer contents; file paths are considered safe to surface
/// because callers already supplied them.
#[derive(Debug)]
#[non_exhaustive]
#[must_use = "errors should be inspected, propagated, or logged"]
pub enum Error {
    /// A platform IO syscall returned an error.
    ///
    /// Wraps the underlying [`std::io::Error`]. Callers needing the
    /// original `io::ErrorKind` can pattern-match the inner value.
    ///
    /// **Code:** `FS-00001`. Caller action: inspect the inner kind; this
    /// is typically a transient condition such as `Interrupted` or a
    /// configuration problem such as `PermissionDenied`.
    Io(std::io::Error),

    /// A supplied path could not be used.
    ///
    /// **Code:** `FS-00002`. Caller action: correct the path. Common
    /// causes include empty segments, embedded NUL bytes, or characters
    /// disallowed on the active platform.
    InvalidPath {
        /// The offending path, as supplied by the caller.
        path: PathBuf,
        /// Human-readable explanation of why the path was rejected.
        reason: String,
    },

    /// A hardware probe failed.
    ///
    /// **Code:** `FS-00003`. Caller action: treat as advisory. The crate
    /// continues to operate with conservative defaults (queue depth 1,
    /// drive kind unknown, no PLP); only call sites that need real
    /// hardware information should treat this as fatal.
    HardwareProbeFailed {
        /// Detail string describing which probe failed and why.
        detail: String,
    },

    /// The requested feature is not available on the active platform.
    ///
    /// **Code:** `FS-00004`. Caller action: select an alternative
    /// strategy, fall back to a portable path, or recompile with the
    /// appropriate feature flag.
    UnsupportedPlatform {
        /// Detail string describing what was requested and why this
        /// platform cannot serve it.
        detail: String,
    },

    /// The requested durability method is not implemented in this release.
    ///
    /// **Code:** `FS-00005`. Caller action: select an available method
    /// ([`crate::Method::Sync`], [`crate::Method::Data`],
    /// [`crate::Method::Mmap`], [`crate::Method::Direct`], or
    /// [`crate::Method::Auto`]). `Method::Journal` is the only
    /// remaining reserved variant in `0.6.x`; planned for `0.7.0`.
    UnsupportedMethod {
        /// The name of the method that was requested.
        method: &'static str,
    },

    /// A Direct IO operation could not satisfy its alignment requirements.
    ///
    /// **Code:** `FS-00006`. Caller action: this is an internal alignment
    /// failure. File a bug if you encounter this — fsys is responsible for
    /// managing alignment transparently on behalf of the caller.
    AlignmentRequired {
        /// Human-readable description of the violated requirement.
        detail: &'static str,
    },

    /// The atomic write-replace sequence failed part-way through.
    ///
    /// **Code:** `FS-00007`. Caller action: inspect `step` to determine
    /// how far the operation progressed. If `step` is `"write"` or
    /// earlier, the original file is unmodified. If `step` is `"rename"`,
    /// the original file may or may not have been replaced. A stale temp
    /// file may remain adjacent to the destination; it is safe to delete.
    AtomicReplaceFailed {
        /// The step that failed (e.g. `"open"`, `"write"`, `"flush"`,
        /// `"rename"`, `"sync_parent"`).
        step: &'static str,
        /// The underlying IO error from the failed step.
        source: std::io::Error,
    },

    /// A directory creation or removal operation failed part-way through.
    ///
    /// **Code:** `FS-00008`. Caller action: the filesystem is in a
    /// partially modified state. Inspect `failed_step` to identify which
    /// sub-operation triggered the error, and `completed_steps` to know
    /// what succeeded before the failure. No rollback is performed; the
    /// caller decides whether to retry, clean up, or accept the partial
    /// state.
    PartialDirectoryOp {
        /// The operation that failed (e.g. `"create /a/b/c"`).
        failed_step: String,
        /// Operations that succeeded before the failure.
        completed_steps: Vec<String>,
    },

    /// A batch operation was submitted to a [`crate::Handle`] that is
    /// being dropped.
    ///
    /// **Code:** `FS-00009`. Caller action: the handle is shutting down;
    /// rebuild a new handle if more IO is needed. This error is only
    /// produced when a batch submit races with `Handle::drop` — it is
    /// effectively unreachable when handle ownership is single-threaded
    /// or properly fenced.
    ShutdownInProgress,

    /// The group-lane queue is full and a non-blocking submission was
    /// rejected.
    ///
    /// **Code:** `FS-00010`. **Reserved variant — never emitted in
    /// `0.4.0`.** The default backpressure mode in `0.4.0` is blocking
    /// submission (callers wait when the queue is full); this variant
    /// is reserved for a future opt-in error-mode (`Builder::backpressure
    /// (BackpressureMode::Error)`) that has not landed yet. Match it to
    /// satisfy exhaustiveness even though it cannot occur today.
    QueueFull,

    /// `io_uring_setup(2)` failed when constructing a per-handle ring.
    ///
    /// **Code:** `FS-00011`. Caller action: the Linux Direct path's
    /// io_uring branch is unavailable for this handle; fsys silently
    /// falls back to the `O_DIRECT` + `pwrite` + `fdatasync` path
    /// (locked decision #1 in `.dev/DECISIONS-0.5.0.md`). The fallback
    /// is observable via [`crate::Handle::active_method`]. This variant
    /// surfaces only when a caller explicitly requests ring diagnostics
    /// — normal handle creation does not return it. Common causes:
    /// kernel < 5.1, `io_uring_setup` disabled by a security profile
    /// (SECCOMP, AppArmor), container runtime restrictions.
    IoUringSetupFailed {
        /// Underlying `io::Error` returned by the failing `io_uring_setup`
        /// (or equivalent) syscall.
        source: std::io::Error,
    },

    /// A memory-mapped IO operation failed.
    ///
    /// **Code:** `FS-00012`. Caller action: when emitted from
    /// [`crate::Method::Mmap`] write/read paths, fsys has already
    /// attempted the documented fallback to [`crate::Method::Sync`].
    /// This variant surfaces only when fallback also fails — typically
    /// because the underlying file is on a filesystem that rejects both
    /// `mmap` and standard `write` (rare; usually a pseudo-filesystem
    /// like `procfs`).
    MmapFailed {
        /// Human-readable explanation of what failed (mapping creation,
        /// `msync`, page-size alignment, etc.).
        reason: String,
    },

    /// The per-handle aligned buffer pool is exhausted and a
    /// non-blocking lease was rejected.
    ///
    /// **Code:** `FS-00013`. **Reserved variant — never emitted in
    /// `0.5.0`.** Default lease semantics block until a buffer is
    /// returned to the pool (mirrors the bounded-queue blocking-submit
    /// contract from `0.4.0` decision #4). This variant is reserved
    /// for a future opt-in error-mode (e.g.
    /// `Builder::buffer_pool_mode(BufferPoolMode::Error)`). Match it
    /// to satisfy exhaustiveness even though it cannot occur today.
    BufferPoolExhausted,

    /// A PLP (Power Loss Protection) probe failed or is unavailable on
    /// this platform.
    ///
    /// **Code:** `FS-00014`. **Informational variant — `0.5.0`'s
    /// public API does not return it.** Per locked decision #3, PLP
    /// probe failures degrade [`crate::hardware::DriveInfo::plp`] to
    /// `Unknown` and log via the metrics placeholder; they do not fail
    /// handle creation. The variant exists in the enum so a future
    /// `probe_plp() -> Result<bool>` API can surface the underlying
    /// reason on request (out of scope for `0.5.0` per follow-up F-8
    /// in `.dev/DECISIONS-0.5.0.md`).
    PlpDetectionUnavailable {
        /// Human-readable explanation: missing capability, unsupported
        /// platform, IOCTL failure, etc.
        detail: String,
    },

    /// NVMe passthrough flush is not supported on the current platform.
    ///
    /// **Code:** `FS-00015`. Caller action: select an alternative
    /// method or accept the platform's standard durability primitive.
    /// macOS does not expose NVMe passthrough; this variant is the
    /// honest fail-fast for callers explicitly requesting
    /// `Method::Direct` with passthrough on macOS. On Linux and
    /// Windows, missing kernel support (Linux < 5.19) or unsupported
    /// hardware also surfaces here.
    NvmePassthroughUnsupported {
        /// Human-readable explanation: which platform, which kernel,
        /// which hardware constraint.
        detail: String,
    },

    /// NVMe passthrough is supported on this platform but the calling
    /// process lacks the privilege to issue raw NVMe commands.
    ///
    /// **Code:** `FS-00016`. Caller action: this is recoverable. The
    /// `Method::Direct` backend silently falls back to the standard
    /// durability primitive (`fdatasync` on Linux,
    /// `FILE_FLAG_WRITE_THROUGH` on Windows) when capability detection
    /// returns this error during the first Direct op. Callers
    /// observing this variant directly are typically diagnostic tools
    /// (`probe_nvme_passthrough() -> Result<bool>`, deferred to
    /// `0.7.0+` per follow-up F-9) that want to know **why** the
    /// fallback happened.
    NvmePassthroughDenied {
        /// Human-readable explanation: which capability check failed,
        /// which permission was missing, which OS error code surfaced.
        detail: String,
    },

    /// An async method was called outside an active tokio runtime.
    ///
    /// **Code:** `FS-00017`. Caller action: ensure the call site is
    /// inside a `#[tokio::main]` function, a `#[tokio::test]`, or
    /// otherwise within a tokio runtime context. fsys's async layer
    /// uses `tokio::task::spawn_blocking` internally and requires a
    /// runtime to drive the spawned task. This error is returned
    /// instead of panicking on `Handle::current()` failure, so callers
    /// observe a graceful, propagable error rather than a process
    /// crash.
    ///
    /// Only emitted when the `async` Cargo feature is enabled.
    AsyncRuntimeRequired,

    /// A glob pattern supplied to [`crate::Handle::find`] could not be
    /// parsed.
    ///
    /// **Code:** `FS-00018`. Caller action: correct the pattern. The
    /// accepted syntax is the `glob` crate's standard:
    /// `*` (any chars except `/`), `**` (any chars including `/`),
    /// `?` (one char), `[abc]` / `[!abc]` (character class),
    /// `{foo,bar}` (alternation). Patterns that escape the base
    /// directory (e.g. `../../etc/passwd`) are rejected with
    /// [`Error::InvalidPath`] instead — this variant covers only
    /// pattern-syntax errors.
    GlobPatternInvalid {
        /// Human-readable explanation of the syntax error.
        reason: String,
    },
}

impl Error {
    /// Returns the stable `FS-XXXXX` code identifying this variant.
    ///
    /// Codes are stable across releases. They never change for an
    /// existing variant; new variants receive new codes.
    ///
    /// # Examples
    ///
    /// ```
    /// use fsys::Error;
    /// use std::io;
    ///
    /// let err = Error::Io(io::Error::from(io::ErrorKind::NotFound));
    /// assert_eq!(err.code(), "FS-00001");
    /// ```
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io(_) => "FS-00001",
            Error::InvalidPath { .. } => "FS-00002",
            Error::HardwareProbeFailed { .. } => "FS-00003",
            Error::UnsupportedPlatform { .. } => "FS-00004",
            Error::UnsupportedMethod { .. } => "FS-00005",
            Error::AlignmentRequired { .. } => "FS-00006",
            Error::AtomicReplaceFailed { .. } => "FS-00007",
            Error::PartialDirectoryOp { .. } => "FS-00008",
            Error::ShutdownInProgress => "FS-00009",
            Error::QueueFull => "FS-00010",
            Error::IoUringSetupFailed { .. } => "FS-00011",
            Error::MmapFailed { .. } => "FS-00012",
            Error::BufferPoolExhausted => "FS-00013",
            Error::PlpDetectionUnavailable { .. } => "FS-00014",
            Error::NvmePassthroughUnsupported { .. } => "FS-00015",
            Error::NvmePassthroughDenied { .. } => "FS-00016",
            Error::AsyncRuntimeRequired => "FS-00017",
            Error::GlobPatternInvalid { .. } => "FS-00018",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "[{}] io error: {}", self.code(), e),
            Error::InvalidPath { path, reason } => write!(
                f,
                "[{}] invalid path {:?}: {}",
                self.code(),
                path.display(),
                reason
            ),
            Error::HardwareProbeFailed { detail } => {
                write!(f, "[{}] hardware probe failed: {}", self.code(), detail)
            }
            Error::UnsupportedPlatform { detail } => {
                write!(f, "[{}] unsupported platform: {}", self.code(), detail)
            }
            Error::UnsupportedMethod { method } => {
                write!(
                    f,
                    "[{}] method '{}' is not implemented in this release",
                    self.code(),
                    method
                )
            }
            Error::AlignmentRequired { detail } => {
                write!(
                    f,
                    "[{}] alignment requirement failed: {}",
                    self.code(),
                    detail
                )
            }
            Error::AtomicReplaceFailed { step, source } => {
                write!(
                    f,
                    "[{}] atomic write-replace failed at step '{}': {}",
                    self.code(),
                    step,
                    source
                )
            }
            Error::PartialDirectoryOp {
                failed_step,
                completed_steps,
            } => {
                write!(
                    f,
                    "[{}] directory op failed at '{}' after {} completed step(s)",
                    self.code(),
                    failed_step,
                    completed_steps.len()
                )
            }
            Error::ShutdownInProgress => {
                write!(
                    f,
                    "[{}] handle is shutting down; batch submission rejected",
                    self.code()
                )
            }
            Error::QueueFull => {
                write!(
                    f,
                    "[{}] group-lane queue is full (reserved variant; never emitted in 0.4.0)",
                    self.code()
                )
            }
            Error::IoUringSetupFailed { source } => {
                write!(f, "[{}] io_uring_setup failed: {}", self.code(), source)
            }
            Error::MmapFailed { reason } => {
                write!(f, "[{}] mmap operation failed: {}", self.code(), reason)
            }
            Error::BufferPoolExhausted => {
                write!(
                    f,
                    "[{}] aligned buffer pool exhausted (reserved variant; never emitted in 0.5.0)",
                    self.code()
                )
            }
            Error::PlpDetectionUnavailable { detail } => {
                write!(f, "[{}] PLP detection unavailable: {}", self.code(), detail)
            }
            Error::NvmePassthroughUnsupported { detail } => {
                write!(
                    f,
                    "[{}] NVMe passthrough unsupported: {}",
                    self.code(),
                    detail
                )
            }
            Error::NvmePassthroughDenied { detail } => {
                write!(f, "[{}] NVMe passthrough denied: {}", self.code(), detail)
            }
            Error::AsyncRuntimeRequired => {
                write!(
                    f,
                    "[{}] async method called outside an active tokio runtime",
                    self.code()
                )
            }
            Error::GlobPatternInvalid { reason } => {
                write!(f, "[{}] invalid glob pattern: {}", self.code(), reason)
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::AtomicReplaceFailed { source, .. } => Some(source),
            Error::IoUringSetupFailed { source } => Some(source),
            Error::InvalidPath { .. }
            | Error::HardwareProbeFailed { .. }
            | Error::UnsupportedPlatform { .. }
            | Error::UnsupportedMethod { .. }
            | Error::AlignmentRequired { .. }
            | Error::PartialDirectoryOp { .. }
            | Error::ShutdownInProgress
            | Error::QueueFull
            | Error::MmapFailed { .. }
            | Error::BufferPoolExhausted
            | Error::PlpDetectionUnavailable { .. }
            | Error::NvmePassthroughUnsupported { .. }
            | Error::NvmePassthroughDenied { .. }
            | Error::AsyncRuntimeRequired
            | Error::GlobPatternInvalid { .. } => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// BatchError
// ──────────────────────────────────────────────────────────────────────────────

/// The error type returned by batch operations
/// (`Handle::write_batch`, `Handle::delete_batch`, `Handle::copy_batch`,
/// and `Batch::commit`).
///
/// `BatchError` reports per-batch failure semantics under decision #5
/// of the `0.4.0` design (independent ops, not transactions). When a
/// batch op fails — whether by returning an `Err` or by panicking — the
/// dispatcher stops processing the current batch, sends the response,
/// and moves on to the next batch in the queue. Subsequent ops in the
/// failing batch are **not attempted**. Ops that succeeded before the
/// failure **are** durable; fsys does **not** roll them back.
///
/// To recover from a `BatchError`, inspect:
/// - [`failed_at`](BatchError::failed_at): the index of the op that
///   failed.
/// - [`completed`](BatchError::completed): the number of ops that
///   completed successfully *before* the failure (always equal to
///   `failed_at` in `0.4.0`; the field is preserved as a structural
///   guarantee for future phases that might allow continuation).
/// - [`source`](BatchError::source): the underlying [`Error`] that
///   describes the failure.
///
/// Callers needing all-or-nothing semantics must layer their own
/// transactional logic on top of fsys, or wait for `Method::Journal`
/// in `0.7.0`.
#[derive(Debug)]
#[non_exhaustive]
#[must_use = "errors should be inspected, propagated, or logged"]
pub struct BatchError {
    /// The zero-based index of the op that failed within its batch.
    pub failed_at: usize,
    /// The number of ops that completed successfully before the failure.
    pub completed: usize,
    /// The underlying error.
    ///
    /// Boxed because `Error` is `non_exhaustive` and may grow large; the
    /// box keeps `BatchError` itself small even when the inner error
    /// carries large payloads (e.g. paths, detail strings, captured
    /// `std::io::Error`s).
    pub source: Box<Error>,
}

impl BatchError {
    /// Returns the inner [`Error`] as a borrowed reference.
    ///
    /// Convenience wrapper over `&*self.source`.
    pub fn inner(&self) -> &Error {
        &self.source
    }

    /// Consumes this `BatchError` and returns the boxed inner [`Error`].
    pub fn into_inner(self) -> Box<Error> {
        self.source
    }
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "batch failed at op {} after {} successful op(s): {}",
            self.failed_at, self.completed, self.source
        )
    }
}

impl std::error::Error for BatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn test_error_code_io_returns_fs00001() {
        let err = Error::Io(io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(err.code(), "FS-00001");
    }

    #[test]
    fn test_error_code_invalid_path_returns_fs00002() {
        let err = Error::InvalidPath {
            path: PathBuf::from("bad"),
            reason: "empty segment".into(),
        };
        assert_eq!(err.code(), "FS-00002");
    }

    #[test]
    fn test_error_code_hardware_probe_returns_fs00003() {
        let err = Error::HardwareProbeFailed {
            detail: "nvme ioctl unavailable".into(),
        };
        assert_eq!(err.code(), "FS-00003");
    }

    #[test]
    fn test_error_code_unsupported_platform_returns_fs00004() {
        let err = Error::UnsupportedPlatform {
            detail: "io_uring requires Linux 5.1+".into(),
        };
        assert_eq!(err.code(), "FS-00004");
    }

    #[test]
    fn test_error_display_unsupported_platform_includes_detail() {
        let err = Error::UnsupportedPlatform {
            detail: "io_uring not available".into(),
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00004]"));
        assert!(s.contains("io_uring not available"));
    }

    #[test]
    fn test_error_display_io_includes_code_and_kind() {
        let err = Error::Io(io::Error::from(io::ErrorKind::NotFound));
        let s = err.to_string();
        assert!(s.starts_with("[FS-00001]"));
        assert!(s.contains("io error"));
    }

    #[test]
    fn test_error_display_invalid_path_does_not_panic_on_unicode() {
        let err = Error::InvalidPath {
            path: PathBuf::from("名前/test"),
            reason: "rejected".into(),
        };
        let s = err.to_string();
        assert!(s.contains("FS-00002"));
    }

    #[test]
    fn test_error_source_io_returns_inner() {
        let inner = io::Error::from(io::ErrorKind::PermissionDenied);
        let err = Error::Io(inner);
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn test_error_source_invalid_path_returns_none() {
        let err = Error::InvalidPath {
            path: PathBuf::from("x"),
            reason: "y".into(),
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_error_from_io_error_converts() {
        let io_err = io::Error::from(io::ErrorKind::Other);
        let err: Error = io_err.into();
        assert_eq!(err.code(), "FS-00001");
    }

    #[test]
    fn test_result_alias_compiles_for_ok_and_err_paths() {
        fn returns_ok() -> Result<u8> {
            Ok(1)
        }
        fn returns_err() -> Result<u8> {
            Err(Error::HardwareProbeFailed {
                detail: "test".into(),
            })
        }
        assert_eq!(returns_ok().ok(), Some(1));
        assert!(returns_err().is_err());
    }

    #[test]
    fn test_error_code_unsupported_method_returns_fs00005() {
        let err = Error::UnsupportedMethod { method: "Mmap" };
        assert_eq!(err.code(), "FS-00005");
    }

    #[test]
    fn test_error_display_unsupported_method_includes_name() {
        let err = Error::UnsupportedMethod { method: "Journal" };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00005]"));
        assert!(s.contains("Journal"));
    }

    #[test]
    fn test_error_code_alignment_required_returns_fs00006() {
        let err = Error::AlignmentRequired {
            detail: "size not a multiple of sector size",
        };
        assert_eq!(err.code(), "FS-00006");
    }

    #[test]
    fn test_error_display_alignment_required_includes_detail() {
        let err = Error::AlignmentRequired {
            detail: "buffer not aligned to 4096",
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00006]"));
        assert!(s.contains("4096"));
    }

    #[test]
    fn test_error_code_atomic_replace_failed_returns_fs00007() {
        let err = Error::AtomicReplaceFailed {
            step: "rename",
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert_eq!(err.code(), "FS-00007");
    }

    #[test]
    fn test_error_display_atomic_replace_includes_step() {
        let err = Error::AtomicReplaceFailed {
            step: "flush",
            source: io::Error::from(io::ErrorKind::Other),
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00007]"));
        assert!(s.contains("flush"));
    }

    #[test]
    fn test_error_source_atomic_replace_returns_inner() {
        let err = Error::AtomicReplaceFailed {
            step: "write",
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn test_error_code_partial_dir_op_returns_fs00008() {
        let err = Error::PartialDirectoryOp {
            failed_step: "create /a/b".into(),
            completed_steps: vec!["create /a".into()],
        };
        assert_eq!(err.code(), "FS-00008");
    }

    #[test]
    fn test_error_display_partial_dir_op_includes_step() {
        let err = Error::PartialDirectoryOp {
            failed_step: "create /a/b/c".into(),
            completed_steps: vec!["create /a".into(), "create /a/b".into()],
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00008]"));
        assert!(s.contains("/a/b/c"));
    }

    #[test]
    fn test_error_source_partial_dir_op_returns_none() {
        let err = Error::PartialDirectoryOp {
            failed_step: "create /x".into(),
            completed_steps: vec![],
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    // ── 0.4.0 additions ──────────────────────────────────────────────────

    #[test]
    fn test_error_code_shutdown_in_progress_returns_fs00009() {
        let err = Error::ShutdownInProgress;
        assert_eq!(err.code(), "FS-00009");
    }

    #[test]
    fn test_error_display_shutdown_in_progress_includes_code() {
        let err = Error::ShutdownInProgress;
        let s = err.to_string();
        assert!(s.starts_with("[FS-00009]"));
        assert!(s.contains("shutting down"));
    }

    #[test]
    fn test_error_source_shutdown_in_progress_returns_none() {
        let err = Error::ShutdownInProgress;
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_error_code_queue_full_returns_fs00010() {
        let err = Error::QueueFull;
        assert_eq!(err.code(), "FS-00010");
    }

    #[test]
    fn test_error_display_queue_full_marked_reserved() {
        let err = Error::QueueFull;
        let s = err.to_string();
        assert!(s.starts_with("[FS-00010]"));
        assert!(s.to_ascii_lowercase().contains("reserved"));
    }

    #[test]
    fn test_error_source_queue_full_returns_none() {
        let err = Error::QueueFull;
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_batch_error_fields_round_trip() {
        let inner = Error::Io(io::Error::from(io::ErrorKind::NotFound));
        let be = BatchError {
            failed_at: 3,
            completed: 3,
            source: Box::new(inner),
        };
        assert_eq!(be.failed_at, 3);
        assert_eq!(be.completed, 3);
        assert_eq!(be.inner().code(), "FS-00001");
    }

    #[test]
    fn test_batch_error_display_includes_indices_and_inner() {
        let inner = Error::HardwareProbeFailed {
            detail: "probe stub".into(),
        };
        let be = BatchError {
            failed_at: 7,
            completed: 7,
            source: Box::new(inner),
        };
        let s = be.to_string();
        assert!(s.contains("op 7"));
        assert!(s.contains("7 successful"));
        assert!(s.contains("FS-00003"));
    }

    #[test]
    fn test_batch_error_implements_std_error_with_inner_source() {
        let inner = Error::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        let be = BatchError {
            failed_at: 0,
            completed: 0,
            source: Box::new(inner),
        };
        let dyn_err: &dyn std::error::Error = &be;
        assert!(dyn_err.source().is_some());
    }

    #[test]
    fn test_batch_error_into_inner_returns_boxed_error() {
        let inner = Error::ShutdownInProgress;
        let be = BatchError {
            failed_at: 0,
            completed: 0,
            source: Box::new(inner),
        };
        let unboxed: Box<Error> = be.into_inner();
        assert_eq!(unboxed.code(), "FS-00009");
    }

    // ── 0.5.0 additions ──────────────────────────────────────────────────

    #[test]
    fn test_error_code_io_uring_setup_failed_returns_fs00011() {
        let err = Error::IoUringSetupFailed {
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert_eq!(err.code(), "FS-00011");
    }

    #[test]
    fn test_error_display_io_uring_setup_failed_includes_source() {
        let err = Error::IoUringSetupFailed {
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00011]"));
        assert!(s.contains("io_uring_setup"));
    }

    #[test]
    fn test_error_source_io_uring_setup_failed_returns_inner() {
        let err = Error::IoUringSetupFailed {
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn test_error_code_mmap_failed_returns_fs00012() {
        let err = Error::MmapFailed {
            reason: "page-size alignment failed".into(),
        };
        assert_eq!(err.code(), "FS-00012");
    }

    #[test]
    fn test_error_display_mmap_failed_includes_reason() {
        let err = Error::MmapFailed {
            reason: "fallback to Sync also failed on procfs".into(),
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00012]"));
        assert!(s.contains("procfs"));
    }

    #[test]
    fn test_error_source_mmap_failed_returns_none() {
        let err = Error::MmapFailed {
            reason: "test".into(),
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_error_code_buffer_pool_exhausted_returns_fs00013() {
        let err = Error::BufferPoolExhausted;
        assert_eq!(err.code(), "FS-00013");
    }

    #[test]
    fn test_error_display_buffer_pool_exhausted_marked_reserved() {
        let err = Error::BufferPoolExhausted;
        let s = err.to_string();
        assert!(s.starts_with("[FS-00013]"));
        assert!(s.to_ascii_lowercase().contains("reserved"));
    }

    #[test]
    fn test_error_source_buffer_pool_exhausted_returns_none() {
        let err = Error::BufferPoolExhausted;
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_error_code_plp_detection_unavailable_returns_fs00014() {
        let err = Error::PlpDetectionUnavailable {
            detail: "CAP_SYS_ADMIN required".into(),
        };
        assert_eq!(err.code(), "FS-00014");
    }

    #[test]
    fn test_error_display_plp_detection_unavailable_includes_detail() {
        let err = Error::PlpDetectionUnavailable {
            detail: "IOKit property missing".into(),
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00014]"));
        assert!(s.contains("IOKit property missing"));
    }

    #[test]
    fn test_error_source_plp_detection_unavailable_returns_none() {
        let err = Error::PlpDetectionUnavailable {
            detail: "test".into(),
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    // ── 0.6.0 additions ──────────────────────────────────────────────────

    #[test]
    fn test_error_code_nvme_passthrough_unsupported_returns_fs00015() {
        let err = Error::NvmePassthroughUnsupported {
            detail: "macOS does not expose IOCTL_STORAGE_PROTOCOL_COMMAND".into(),
        };
        assert_eq!(err.code(), "FS-00015");
    }

    #[test]
    fn test_error_display_nvme_passthrough_unsupported_includes_detail() {
        let err = Error::NvmePassthroughUnsupported {
            detail: "kernel < 5.19".into(),
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00015]"));
        assert!(s.contains("kernel < 5.19"));
    }

    #[test]
    fn test_error_source_nvme_passthrough_unsupported_returns_none() {
        let err = Error::NvmePassthroughUnsupported {
            detail: "test".into(),
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_error_code_nvme_passthrough_denied_returns_fs00016() {
        let err = Error::NvmePassthroughDenied {
            detail: "EACCES on /dev/nvme0".into(),
        };
        assert_eq!(err.code(), "FS-00016");
    }

    #[test]
    fn test_error_display_nvme_passthrough_denied_includes_detail() {
        let err = Error::NvmePassthroughDenied {
            detail: "ERROR_ACCESS_DENIED on STORAGE_PROTOCOL_COMMAND".into(),
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00016]"));
        assert!(s.contains("STORAGE_PROTOCOL_COMMAND"));
    }

    #[test]
    fn test_error_source_nvme_passthrough_denied_returns_none() {
        let err = Error::NvmePassthroughDenied {
            detail: "test".into(),
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_error_code_async_runtime_required_returns_fs00017() {
        let err = Error::AsyncRuntimeRequired;
        assert_eq!(err.code(), "FS-00017");
    }

    #[test]
    fn test_error_display_async_runtime_required_mentions_tokio() {
        let err = Error::AsyncRuntimeRequired;
        let s = err.to_string();
        assert!(s.starts_with("[FS-00017]"));
        assert!(s.to_ascii_lowercase().contains("tokio"));
    }

    #[test]
    fn test_error_source_async_runtime_required_returns_none() {
        let err = Error::AsyncRuntimeRequired;
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_error_code_glob_pattern_invalid_returns_fs00018() {
        let err = Error::GlobPatternInvalid {
            reason: "unmatched bracket".into(),
        };
        assert_eq!(err.code(), "FS-00018");
    }

    #[test]
    fn test_error_display_glob_pattern_invalid_includes_reason() {
        let err = Error::GlobPatternInvalid {
            reason: "stray '['".into(),
        };
        let s = err.to_string();
        assert!(s.starts_with("[FS-00018]"));
        assert!(s.contains("stray"));
    }

    #[test]
    fn test_error_source_glob_pattern_invalid_returns_none() {
        let err = Error::GlobPatternInvalid {
            reason: "test".into(),
        };
        assert!(std::error::Error::source(&err).is_none());
    }
}
