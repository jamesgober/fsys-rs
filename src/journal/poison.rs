//! Sticky failure state for a journal (1.1.1).
//!
//! Once a write, flush or fsync fails, the journal can no longer
//! tell which acknowledged appends reached the file:
//!
//! - A failed positioned write leaves a hole at an LSN range
//!   that later appends have already been handed past.
//! - A failed Direct-IO slot flush loses a whole slot of
//!   records whose `append` calls already returned `Ok`.
//! - A failed `fsync` / `fdatasync` may have dropped dirty
//!   pages. On Linux a retried call can then report success for
//!   data that never reached the device, so retrying is not a
//!   recovery path.
//!
//! The journal therefore poisons itself on the first such
//! failure. Every later append and every `sync_through` whose
//! target is not already durable returns an error. Recovery is
//! to drop the handle, reopen the journal (the reopen scan stops
//! at the first hole or torn frame) and replay from there.

use crate::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Sticky "a write or sync failed" flag plus the first cause.
pub(crate) struct Poison {
    poisoned: AtomicBool,
    /// Kind and message of the first failure, for the error
    /// returned to every later caller.
    cause: OnceLock<(std::io::ErrorKind, String)>,
}

impl Poison {
    /// A healthy (unpoisoned) state.
    pub(crate) const fn new() -> Self {
        Self {
            poisoned: AtomicBool::new(false),
            cause: OnceLock::new(),
        }
    }

    /// Returns `true` once [`Self::set`] has been called.
    #[inline]
    pub(crate) fn is_set(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Returns the poison error if the journal is poisoned.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] carrying the kind of the first failure and a
    /// message that names it and tells the caller to reopen.
    #[inline]
    pub(crate) fn check(&self) -> Result<()> {
        if self.is_set() {
            return Err(self.error());
        }
        Ok(())
    }

    /// Poisons the journal. The first cause wins; later calls only
    /// keep the flag set.
    pub(crate) fn set(&self, cause: &Error) {
        let entry = match cause {
            Error::Io(e) => (e.kind(), e.to_string()),
            other => (std::io::ErrorKind::Other, other.to_string()),
        };
        let _ = self.cause.set(entry);
        self.poisoned.store(true, Ordering::Release);
    }

    #[cold]
    fn error(&self) -> Error {
        let (kind, message) = match self.cause.get() {
            Some((kind, message)) => (*kind, message.as_str()),
            None => (std::io::ErrorKind::Other, "unknown cause"),
        };
        Error::Io(std::io::Error::new(
            kind,
            format!(
                "journal is poisoned by an earlier write or sync failure ({message}); \
                 appends since then may not be on disk, reopen the journal to recover"
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_poison_new_check_returns_ok() {
        let p = Poison::new();
        assert!(!p.is_set());
        assert!(p.check().is_ok());
    }

    #[test]
    fn test_poison_set_check_returns_first_cause() {
        let p = Poison::new();
        p.set(&Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disk full",
        )));
        p.set(&Error::Io(std::io::Error::other("second failure")));
        assert!(p.is_set());
        match p.check() {
            Err(Error::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
                let msg = e.to_string();
                assert!(msg.contains("disk full"), "{msg}");
                assert!(!msg.contains("second failure"), "{msg}");
                assert!(msg.contains("reopen"), "{msg}");
            }
            other => panic!("expected poison error, got {other:?}"),
        }
    }

    #[test]
    fn test_poison_non_io_cause_maps_to_other_kind() {
        let p = Poison::new();
        p.set(&Error::QueueFull);
        match p.check() {
            Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::Other),
            other => panic!("expected poison error, got {other:?}"),
        }
    }
}
