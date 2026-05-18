//! Journal backend abstraction (1.1.0).
//!
//! 1.0 shipped a single journal implementation: the kernel + io_uring
//! path that lives inline in [`super::JournalHandle`]. 1.1.0 introduces
//! pluggable backends so SPDK (kernel-bypass, user-space NVMe access)
//! can plug in beside the kernel path without duplicating the journal
//! contract.
//!
//! The trait itself is the **stable public observability surface** in
//! 1.1.0:
//!
//! - [`JournalBackendKind`] — which backend is live for a journal.
//! - [`JournalBackendHealth`] — running counters for monitoring.
//! - [`JournalBackendInfo`] — startup-time selection trail showing
//!   which backends were considered, which was chosen, and why.
//!
//! ## Status of the trait abstraction
//!
//! 1.1.0 defines the trait + observability types and surfaces them
//! through [`super::JournalHandle::backend_kind`] /
//! [`super::JournalHandle::backend_health`] /
//! [`super::JournalHandle::backend_info`]. The existing
//! [`super::JournalHandle`] implementation is the *current*
//! kernel-path backend and reports its own backend identity
//! honestly via those accessors — there is no internal indirection
//! through `Box<dyn JournalBackend>` yet.
//!
//! Trait extraction (moving the in-line append/sync paths behind a
//! `Box<dyn JournalBackend>` so the kernel and SPDK implementations
//! are interchangeable at the type level) lands in a follow-up
//! session — that refactor touches every method on the load-bearing
//! journal hot path and demands careful, isolated testing against
//! the existing emdb production workload. Splitting it from the
//! observability + capability work in 1.1.0 keeps the public 1.x
//! surface frozen while the internal restructuring happens with
//! regression discipline.
//!
//! Consumers can write **forward-compatible** code today against the
//! public types here — they will continue to work unchanged when the
//! trait extraction completes.

use crate::journal::Lsn;
use crate::Result;
use std::time::SystemTime;

/// The interface every journal backend implements.
///
/// **Trait stability status:** the trait shape itself is provisional
/// until the kernel-path extraction completes (see module-level
/// comment). Public types referenced by the trait
/// ([`JournalBackendKind`], [`JournalBackendHealth`],
/// [`JournalBackendInfo`]) and the accessor methods on
/// [`super::JournalHandle`] that surface them are stable in 1.x —
/// they are what consumers should depend on. The trait method
/// signatures may be refined in 1.2 / 1.3 as additional backends
/// land; we will go through the deprecation cycle described in
/// `docs/STABILITY-1.0.md` if any signature changes.
///
/// # Implementations
///
/// - `KernelJournalBackend` (planned for the 1.2 refactor) — the
///   existing in-line implementation, extracted behind the trait.
/// - `SpdkJournalBackend` (planned for the 1.1.x companion crate
///   `fsys-spdk`) — the kernel-bypass NVMe path.
pub trait JournalBackend: Send + Sync {
    /// Append a single record. Returns the LSN assigned by the
    /// backend.
    ///
    /// # Errors
    ///
    /// Returns whatever the backend's underlying append path
    /// produces — typically [`crate::Error::Io`],
    /// [`crate::Error::ShutdownInProgress`], or a backend-specific
    /// variant.
    fn append(&self, record: &[u8]) -> Result<Lsn>;

    /// Append a batch of records. All records become durable together
    /// at the next [`Self::flush`] call against any of the returned
    /// LSNs (or any later LSN).
    ///
    /// # Errors
    ///
    /// Same shape as [`Self::append`].
    fn append_batch(&self, records: &[&[u8]]) -> Result<Vec<Lsn>>;

    /// Force durability of every record up to and including `up_to`.
    /// Concurrent calls coalesce into one platform syscall via the
    /// backend's group-commit coordinator.
    ///
    /// # Errors
    ///
    /// Surfaces the backend's underlying sync failure when the
    /// platform's durability primitive returns an error.
    fn flush(&self, up_to: Lsn) -> Result<()>;

    /// Reads a single record at the given LSN. Used during recovery
    /// and by `JournalReader::read_at_lsn`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Io`] for backend-level IO failures.
    /// Returns the backend's record-decode error variant (typically
    /// surfaced via the per-backend frame format) on a malformed
    /// record.
    fn read(&self, lsn: Lsn) -> Result<Vec<u8>>;

    /// Backend identity for observability.
    fn backend_kind(&self) -> JournalBackendKind;

    /// Running health counters for monitoring.
    fn health(&self) -> JournalBackendHealth;
}

/// Concrete backend identity reported via [`JournalBackend::backend_kind`].
///
/// The enum is `#[non_exhaustive]` so adding new backend identities
/// in a future minor release (PMEM, RDMA, etc.) is non-breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JournalBackendKind {
    /// Kernel path with `io_uring` submission + completion (Linux
    /// `Method::Direct` + `Method::Auto` selection).
    KernelIoUring,
    /// Kernel path with `O_DIRECT` + synchronous `pwrite` + manual
    /// `fdatasync` (Linux without `io_uring`, macOS, Windows
    /// `FILE_FLAG_NO_BUFFERING`).
    KernelDirect,
    /// Kernel path with buffered IO + `fdatasync` / equivalent
    /// (the universal fallback; default mode of [`super::JournalHandle`]
    /// when no `JournalOptions::direct(true)` is set).
    KernelBuffered,
    /// SPDK kernel-bypass backend — Linux + `fsys-spdk` feature.
    /// Not selectable in 1.1.0 (the implementation lives in a
    /// companion crate that is in scaffold state); reserved here
    /// so consumers can pattern-match against it today.
    Spdk,
}

impl JournalBackendKind {
    /// Returns the canonical lowercase name for the backend. Used
    /// in selection-reason strings on [`JournalBackendInfo`] and in
    /// the observability accessor on [`super::JournalHandle`].
    #[must_use]
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            JournalBackendKind::KernelIoUring => "kernel-io-uring",
            JournalBackendKind::KernelDirect => "kernel-direct",
            JournalBackendKind::KernelBuffered => "kernel-buffered",
            JournalBackendKind::Spdk => "spdk",
        }
    }

    /// Returns `true` when the backend is a kernel-path backend
    /// (anything except [`JournalBackendKind::Spdk`]).
    #[must_use]
    #[inline]
    pub const fn is_kernel(self) -> bool {
        matches!(
            self,
            JournalBackendKind::KernelIoUring
                | JournalBackendKind::KernelDirect
                | JournalBackendKind::KernelBuffered
        )
    }
}

impl std::fmt::Display for JournalBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Running health counters for a journal backend.
///
/// Returned by [`super::JournalHandle::backend_health`] and
/// [`JournalBackend::health`]. The counters are snapshots; consumers
/// that want trend data should poll on a fixed interval and diff
/// against the previous snapshot.
///
/// All counters except the latency percentiles are monotonically
/// non-decreasing for the lifetime of the journal. Latency
/// percentiles are sliding-window estimates and may decrease as
/// older samples roll out of the window.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct JournalBackendHealth {
    /// Which backend produced this snapshot.
    pub backend: JournalBackendKind,
    /// Current in-flight submission count (records appended but not
    /// yet acknowledged as durable by the backend's storage stack).
    pub queue_depth_current: usize,
    /// Maximum value [`Self::queue_depth_current`] has reached
    /// since the journal opened. Useful for sizing.
    pub queue_depth_max: usize,
    /// Recent appends-per-second rate. Sliding-window estimate; the
    /// exact window size is backend-specific. May be `0` when the
    /// journal has just been opened or has been idle.
    pub appends_per_second: u64,
    /// Average append latency in microseconds, over the same sliding
    /// window as [`Self::appends_per_second`].
    pub avg_append_latency_us: u64,
    /// p99 append latency in microseconds.
    pub p99_append_latency_us: u64,
    /// Number of append calls that returned `Err`. Includes both
    /// transient errors (e.g. `EAGAIN`) and durable errors. Monotonic.
    pub failed_appends: u64,
}

impl JournalBackendHealth {
    /// Constructs a zero-counter health snapshot for `backend`.
    ///
    /// Used by backends that don't yet emit detailed counters (the
    /// kernel path's pre-instrumented baseline) so the accessor on
    /// [`super::JournalHandle`] always has a well-formed value to
    /// return rather than `Option<_>`.
    #[must_use]
    #[inline]
    pub const fn empty(backend: JournalBackendKind) -> Self {
        Self {
            backend,
            queue_depth_current: 0,
            queue_depth_max: 0,
            appends_per_second: 0,
            avg_append_latency_us: 0,
            p99_append_latency_us: 0,
            failed_appends: 0,
        }
    }
}

/// Verbose selection trail produced at journal-open time.
///
/// Returned by [`super::JournalHandle::backend_info`]. Operators
/// **must** be able to verify which backend is actually serving a
/// journal — silently falling through to the kernel path when SPDK
/// was requested would invalidate downstream performance
/// expectations. This struct is that verification surface.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct JournalBackendInfo {
    /// The backend currently serving this journal.
    pub selected: JournalBackendKind,
    /// Plain-English description of why this backend was chosen
    /// (e.g. `"Method::Auto resolved to kernel-io-uring (Linux 6.8,
    /// io_uring available)"`).
    pub selection_reason: String,
    /// Backends considered during selection and skipped, with the
    /// reason. Allows ops to verify that SPDK was tried before
    /// falling through to the kernel path (and to see exactly which
    /// SPDK precondition failed).
    pub fallbacks_skipped: Vec<(JournalBackendKind, String)>,
    /// Wall-clock time the journal opened. Useful for correlating
    /// with system logs and metric backends.
    pub opened_at: SystemTime,
}

impl JournalBackendInfo {
    /// Constructs a single-selection info record with no fallback
    /// trail. Used by the kernel-path implementation when no
    /// alternative was considered (e.g. user explicitly passed a
    /// non-`Auto` method).
    #[must_use]
    pub fn single(selected: JournalBackendKind, selection_reason: impl Into<String>) -> Self {
        Self {
            selected,
            selection_reason: selection_reason.into(),
            fallbacks_skipped: Vec::new(),
            opened_at: SystemTime::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_kind_as_str_round_trip_via_match() {
        let labels: Vec<&str> = [
            JournalBackendKind::KernelIoUring,
            JournalBackendKind::KernelDirect,
            JournalBackendKind::KernelBuffered,
            JournalBackendKind::Spdk,
        ]
        .iter()
        .map(|k| k.as_str())
        .collect();
        assert_eq!(
            labels,
            vec![
                "kernel-io-uring",
                "kernel-direct",
                "kernel-buffered",
                "spdk"
            ]
        );
    }

    #[test]
    fn test_backend_kind_display_matches_as_str() {
        for k in [
            JournalBackendKind::KernelIoUring,
            JournalBackendKind::KernelDirect,
            JournalBackendKind::KernelBuffered,
            JournalBackendKind::Spdk,
        ] {
            assert_eq!(k.to_string(), k.as_str());
        }
    }

    #[test]
    fn test_backend_kind_is_kernel_classifies_correctly() {
        assert!(JournalBackendKind::KernelIoUring.is_kernel());
        assert!(JournalBackendKind::KernelDirect.is_kernel());
        assert!(JournalBackendKind::KernelBuffered.is_kernel());
        assert!(!JournalBackendKind::Spdk.is_kernel());
    }

    #[test]
    fn test_backend_health_empty_constructor_zeroes_counters() {
        let h = JournalBackendHealth::empty(JournalBackendKind::KernelBuffered);
        assert_eq!(h.backend, JournalBackendKind::KernelBuffered);
        assert_eq!(h.queue_depth_current, 0);
        assert_eq!(h.queue_depth_max, 0);
        assert_eq!(h.appends_per_second, 0);
        assert_eq!(h.avg_append_latency_us, 0);
        assert_eq!(h.p99_append_latency_us, 0);
        assert_eq!(h.failed_appends, 0);
    }

    #[test]
    fn test_backend_info_single_no_fallbacks() {
        let info = JournalBackendInfo::single(
            JournalBackendKind::KernelBuffered,
            "explicit Method::Sync request",
        );
        assert_eq!(info.selected, JournalBackendKind::KernelBuffered);
        assert!(info.selection_reason.contains("Method::Sync"));
        assert!(info.fallbacks_skipped.is_empty());
        // opened_at is sensible (within the last second).
        let elapsed = info
            .opened_at
            .elapsed()
            .unwrap_or(std::time::Duration::from_secs(0));
        assert!(elapsed < std::time::Duration::from_secs(2));
    }
}
