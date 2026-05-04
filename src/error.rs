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
    /// [`crate::Method::Direct`], or [`crate::Method::Auto`]).
    /// `Method::Mmap` is planned for `0.5.0`; `Method::Journal` for `0.7.0`.
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
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::AtomicReplaceFailed { source, .. } => Some(source),
            Error::InvalidPath { .. }
            | Error::HardwareProbeFailed { .. }
            | Error::UnsupportedPlatform { .. }
            | Error::UnsupportedMethod { .. }
            | Error::AlignmentRequired { .. }
            | Error::PartialDirectoryOp { .. } => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
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
}
