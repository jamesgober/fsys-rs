//! Write-ahead-log / journal substrate (0.8.0).
//!
//! A journal is an append-only log file with explicit
//! group-commit durability semantics. Unlike `Handle::write`'s
//! atomic-replace pattern (5–7 syscalls per write, fsync per
//! call), a journal is opened *once*, appends are issued without
//! per-call fsync, and durability is established explicitly via
//! [`JournalHandle::sync_through`]. This is the primitive every
//! serious database storage engine uses for the WAL — it gets
//! you to millions of durable writes/sec.
//!
//! ## When to use the journal
//!
//! Use the journal when:
//!
//! - You're building a database, queue, or ledger that needs
//!   high-throughput durable writes with **commit-LSN
//!   semantics** (durability is a sequence point, not a
//!   per-write flush).
//! - You need **group-commit batching** — N appends amortised
//!   across one fsync.
//! - You've already opened the target file conceptually (you're
//!   appending to a single log, not replacing N files).
//!
//! Use [`Handle::write`](crate::Handle::write) instead when:
//!
//! - You need atomic-replace semantics (the file is either
//!   entirely the old payload or entirely the new payload at
//!   every observable point).
//! - You're updating individual files (config files, package
//!   manifests, document state).
//! - Throughput is not the dominant concern; correctness +
//!   atomicity are.
//!
//! ## Concurrency
//!
//! [`JournalHandle`] is `Send + Sync` and can be shared across
//! threads via [`std::sync::Arc`]. Concurrent appends from
//! multiple threads serialise through an atomic LSN reservation
//! (no mutex on the append path); the underlying `pwrite` calls
//! are concurrent-safe per POSIX. Concurrent calls to
//! [`sync_through`](JournalHandle::sync_through) are
//! group-committed: only one `fsync` syscall runs at a time;
//! all callers waiting for an LSN ≤ the synced frontier wake
//! immediately when the in-flight fsync completes.
//!
//! ## LSN model
//!
//! [`Lsn`] is the byte-offset of the *next* write position
//! after a record. So if you append a 100-byte record starting
//! at offset 1000, [`append`](JournalHandle::append) returns
//! `Lsn(1100)`. Calling
//! [`sync_through(Lsn(1100))`](JournalHandle::sync_through)
//! ensures every byte from offset 0 through 1099 is on stable
//! storage.
//!
//! LSNs are monotonic per-handle. They reset to `Lsn(0)` only
//! when the underlying file is truncated or recreated.

pub(crate) mod format;
pub(crate) mod log_buffer;
pub mod options;
pub mod reader;

pub use options::JournalOptions;
pub use reader::{JournalIter, JournalReader, JournalRecord, JournalTailState};

use crate::{Error, Result};
use log_buffer::LogBuffer;
use std::fs::{File, OpenOptions};
use std::io::Seek;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Log sequence number — byte-offset of the next write position
/// in the journal's underlying file.
///
/// Returned by [`JournalHandle::append`]; consumed by
/// [`JournalHandle::sync_through`]. Monotonic per-handle,
/// transparent ordering (`Lsn(100) < Lsn(200)` ⟺ the first
/// record was appended before the second).
///
/// `Lsn(0)` is the start-of-journal sentinel — equivalent to
/// "nothing has been appended yet."
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

impl Lsn {
    /// The start-of-journal sentinel. Equivalent to `Lsn(0)` and
    /// `Lsn::default()`.
    pub const ZERO: Lsn = Lsn(0);

    /// Returns the LSN's underlying byte offset.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Lsn({})", self.0)
    }
}

/// Append-only log file with explicit group-commit durability.
///
/// Open via [`Handle::journal`](crate::Handle::journal). Share
/// across threads via [`Arc`](std::sync::Arc).
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use fsys::builder;
///
/// # fn main() -> fsys::Result<()> {
/// let fs = builder().build()?;
/// let log = Arc::new(fs.journal("/var/log/app.wal")?);
///
/// // Append several records — no fsync, no syscall amplification.
/// let _lsn1 = log.append(b"record 1")?;
/// let _lsn2 = log.append(b"record 2")?;
/// let lsn3 = log.append(b"record 3")?;
///
/// // Group-commit fsync — one syscall covers all three appends.
/// log.sync_through(lsn3)?;
/// # Ok(())
/// # }
/// ```
pub struct JournalHandle {
    /// The underlying file — held *unwrapped*, no `Mutex` on the
    /// append hot path. `std::fs::File` is `Send + Sync`, and
    /// platform `pwrite` (Linux/macOS) / `WriteFile` with offset
    /// (Windows) is concurrent-safe per call when each call writes
    /// to a distinct offset (which is the LSN-reservation
    /// invariant). 0.8.0 R-1 tier-2: removed the `Mutex<File>`
    /// from the append path; concurrent appends from N threads
    /// no longer serialise through a lock.
    pub(crate) file: File,
    /// Highest reserved LSN. Atomically advanced by [`Self::append`]
    /// via `fetch_add(record.len())`; never decreases.
    pub(crate) next_lsn: AtomicU64,
    /// Highest LSN known durable on stable storage. Updated by
    /// [`Self::sync_through`] after a successful fsync; never decreases.
    pub(crate) synced_lsn: AtomicU64,
    /// Held only during fsync to coalesce concurrent group-commit
    /// callers into one syscall. Not on the append hot path.
    pub(crate) sync_gate: Mutex<()>,
    /// Path the journal was opened at — for diagnostics / errors.
    /// Not used on the hot path.
    #[allow(dead_code)]
    path: std::path::PathBuf,
    /// Lazy native io_uring substrate — Linux + `async` feature
    /// only. Constructed on first `append_async` /
    /// `sync_through_async` call inside a tokio runtime context;
    /// `Some(None)` after a construction failure (e.g. io_uring
    /// unavailable) so subsequent async calls fall back to
    /// `spawn_blocking` without retrying. Tier-3 (R-1 follow-up):
    /// when populated, async appends submit `IORING_OP_WRITE`
    /// SQEs and `sync_through_async` submits
    /// `IORING_OP_FSYNC(DATASYNC)` SQEs through the same ring,
    /// eliminating the `spawn_blocking` thread-pool hop.
    #[cfg(all(target_os = "linux", feature = "async"))]
    pub(crate) native_ring: std::sync::OnceLock<
        Option<std::sync::Arc<crate::async_io::completion_driver::AsyncIoUring>>,
    >,
    /// Direct-IO mode flag. `true` when the journal was opened with
    /// [`JournalOptions::direct(true)`]. Determines whether the
    /// append/sync paths route through [`Self::log_buffer`] (mutex-
    /// serialised log-buffer pattern) or the lock-free `pwrite`
    /// path used by buffered-mode journals.
    pub(crate) direct: bool,
    /// In-memory sector-aligned log buffer. `Some(_)` exclusively
    /// when `direct = true`; `None` otherwise. Mutex-protected
    /// because direct-mode appends serialise into a single shared
    /// buffer (the InnoDB / WiredTiger pattern). Buffered-mode
    /// journals retain their lock-free fast path.
    pub(crate) log_buffer: Option<Mutex<LogBuffer>>,
}

impl JournalHandle {
    /// Opens the journal at `path` for append using default
    /// [`JournalOptions`]. Equivalent to
    /// [`Self::open_with_options`] with `JournalOptions::default()`.
    ///
    /// Called via [`Handle::journal`] — `pub(crate)` because
    /// the public entry point lives on [`Handle`] for path-root
    /// resolution.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::open_with_options(path, JournalOptions::default())
    }

    /// Opens the journal at `path` honoring `options`.
    ///
    /// **Buffered mode** (`options.direct == false`, the default):
    /// the file is opened via standard `OpenOptions`, the
    /// lock-free LSN reservation + concurrent `pwrite` path is
    /// active, and resume sets `next_lsn` to the existing file
    /// size.
    ///
    /// **Direct mode** (`options.direct == true`): the file is
    /// opened with the platform's Direct-IO flag (`O_DIRECT` /
    /// `F_NOCACHE` / `FILE_FLAG_NO_BUFFERING`). An in-memory
    /// sector-aligned log buffer is allocated; appends serialise
    /// into the buffer (mutex-protected) and flush in
    /// sector-aligned chunks. Resume scans the existing file
    /// to find the LSN immediately past the last cleanly-decoded
    /// frame and resumes there — partial trailing sector content
    /// is rehydrated into the buffer so subsequent flushes
    /// overwrite the zero-pad cleanly.
    pub(crate) fn open_with_options(path: &Path, options: JournalOptions) -> Result<Self> {
        if options.direct {
            Self::open_direct(path, options)
        } else {
            Self::open_buffered(path)
        }
    }

    /// Buffered-mode constructor. Lock-free append, lock-free
    /// LSN reservation, group-commit fsync. This is the default
    /// path when [`JournalOptions::direct`] is not set.
    fn open_buffered(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(Error::Io)?;

        // Resume: next_lsn = current file length. Seek to end so
        // that any sneaky `write()` (which we don't use, but
        // belt-and-braces) lands at the right place.
        let len = file.seek(std::io::SeekFrom::End(0)).map_err(Error::Io)?;

        Ok(Self {
            file,
            next_lsn: AtomicU64::new(len),
            synced_lsn: AtomicU64::new(len),
            sync_gate: Mutex::new(()),
            path: path.to_path_buf(),
            #[cfg(all(target_os = "linux", feature = "async"))]
            native_ring: std::sync::OnceLock::new(),
            direct: false,
            log_buffer: None,
        })
    }

    /// Direct-mode constructor. Opens the file with the platform's
    /// `O_DIRECT` analog and allocates the sector-aligned log
    /// buffer. On reopen, scans the existing file for the
    /// last-clean LSN and rehydrates the partial trailing sector
    /// into the buffer.
    fn open_direct(path: &Path, options: JournalOptions) -> Result<Self> {
        // Resolve the resume cursor by scanning the existing file
        // (if any) for the last cleanly-decoded frame's end LSN.
        let resume_lsn = if path.exists() {
            scan_clean_end(path)?
        } else {
            0
        };

        let sector_size = crate::platform::probe_sector_size(path);
        // Open the journal file with the platform's Direct-IO
        // flag. `open_direct_journal` returns
        // `(file, direct_active)`; `direct_active = false` means
        // the filesystem rejected the flag and we silently fell
        // back to a buffered handle (still functional, observable
        // via `is_direct_active`).
        let (file, direct_active) = open_direct_journal(path, sector_size)?;

        // If Direct-IO was rejected by the filesystem
        // (`open_direct_journal` returned `direct_active = false`),
        // fall back to the buffered path. We do NOT silently lose
        // the "direct" intent — the caller can observe via
        // [`Self::is_direct_active`].
        let log_buffer = if direct_active {
            // Allocate the log buffer. Resume puts `flush_pos` at
            // the largest sector boundary ≤ resume_lsn; the buffer
            // is primed with the partial trailing sector content
            // (so subsequent flushes overwrite the zero-pad
            // cleanly).
            let cap_bytes = options.log_buffer_kib.saturating_mul(1024);
            let mut buf = LogBuffer::new(cap_bytes, sector_size, 0)?;
            if resume_lsn > 0 {
                rehydrate_log_buffer(&mut buf, &file, sector_size, resume_lsn)?;
            }
            Some(Mutex::new(buf))
        } else {
            None
        };

        Ok(Self {
            file,
            next_lsn: AtomicU64::new(resume_lsn),
            synced_lsn: AtomicU64::new(resume_lsn),
            sync_gate: Mutex::new(()),
            path: path.to_path_buf(),
            #[cfg(all(target_os = "linux", feature = "async"))]
            native_ring: std::sync::OnceLock::new(),
            direct: direct_active,
            log_buffer,
        })
    }

    /// Returns `true` when this journal is using the Direct-IO
    /// log-buffer path. Returns `false` for buffered-mode journals
    /// AND for journals that requested direct mode but had it
    /// silently downgraded to buffered (filesystem rejected
    /// `O_DIRECT`).
    #[must_use]
    pub fn is_direct_active(&self) -> bool {
        self.direct
    }

    /// Appends `record` to the journal and returns the LSN
    /// immediately after this record (i.e. the next-write position).
    ///
    /// Does **not** fsync. Multiple threads may call `append`
    /// concurrently — the LSN reservation is a single
    /// `AtomicU64::fetch_add` (no mutex on the hot path); the
    /// underlying `pwrite` calls are concurrent-safe per POSIX
    /// for typical record sizes. For sub-page records (≤ 4 KiB
    /// typically) `pwrite` is atomic per call; larger records
    /// are looped on partial writes inside the platform layer.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] on the underlying write failure.
    pub fn append(&self, record: &[u8]) -> Result<Lsn> {
        #[cfg(feature = "tracing")]
        let _span = tracing::trace_span!(
            "fsys::journal::append",
            payload_bytes = record.len(),
            direct = self.direct,
        )
        .entered();

        if let Some(buffer_mutex) = &self.log_buffer {
            // Direct-IO log-buffer path. Mutex-serialised: one
            // appender at a time copies its frame into the shared
            // buffer. This trades the lock-free fast path for
            // sector-aligned zero-copy DMA writes.
            let mut buf = buffer_mutex.lock().unwrap_or_else(|p| p.into_inner());
            let (_start, end) = buf.append_frame(&self.file, record)?;
            // Update next_lsn under the lock so observers see a
            // consistent (next_lsn, buffer state) pair.
            self.next_lsn.store(end, Ordering::Release);
            #[cfg(feature = "tracing")]
            tracing::trace!(end_lsn = end, "direct append complete");
            return Ok(Lsn(end));
        }

        // Buffered mode: the lock-free 0.8.0 path.
        //
        // Encode the frame: 12 bytes of overhead (magic +
        // length + crc32c) wrap the user's payload. Uniform
        // framing is load-bearing — even zero-length records
        // produce a 12-byte header-only frame so the reader's
        // forward-iteration invariant holds. The encode step
        // also bounds-checks the record length against
        // `FRAME_MAX_PAYLOAD` (256 MiB).
        let frame = format::encode_frame_owned(record)?;
        let frame_len = frame.len() as u64;

        // Reserve a slot for the entire frame. The LSN
        // semantics: caller-visible LSN is the byte offset
        // *immediately past* this frame — i.e. the start of the
        // next append. Internally, the file's byte content is
        // framed; readers using `JournalReader` walk the
        // frames forward and yield payloads.
        //
        // fetch_add is `AcqRel` so concurrent appenders see
        // consistent reservation order.
        let start = self.next_lsn.fetch_add(frame_len, Ordering::AcqRel);
        let end = start + frame_len;

        // Lock-free hot path: pwrite directly against
        // `&self.file`. `File` is `Send + Sync`; pwrite is
        // concurrent-safe per POSIX when each call writes to a
        // distinct offset (which the LSN reservation
        // guarantees). No mutex.
        crate::platform::write_at(&self.file, start, &frame)?;

        Ok(Lsn(end))
    }

    /// Forces all bytes up to `lsn` to stable storage.
    ///
    /// Group-committed: concurrent calls coalesce into a single
    /// `fsync` syscall. Callers waiting for an LSN ≤ the synced
    /// frontier return immediately when the in-flight fsync
    /// completes.
    ///
    /// `sync_through(Lsn::ZERO)` is a no-op — nothing has been
    /// appended below offset zero. `sync_through(lsn)` where
    /// `lsn` exceeds the highest appended LSN syncs the
    /// currently-appended frontier (whatever has been appended
    /// up to "now").
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] if the underlying `fsync`/`fdatasync`
    ///   fails.
    pub fn sync_through(&self, lsn: Lsn) -> Result<()> {
        #[cfg(feature = "tracing")]
        let _span = tracing::trace_span!(
            "fsys::journal::sync_through",
            target_lsn = lsn.0,
            direct = self.direct,
        )
        .entered();

        // Fast path: already synced?
        if self.synced_lsn.load(Ordering::Acquire) >= lsn.0 {
            #[cfg(feature = "tracing")]
            tracing::trace!(
                synced_lsn = self.synced_lsn.load(Ordering::Acquire),
                "fast-path sync skipped"
            );
            return Ok(());
        }

        // Slow path: take the sync gate. Only one thread does
        // fsync at a time; everyone else waits.
        let _guard = self.sync_gate.lock().unwrap_or_else(|p| p.into_inner());

        // Re-check after gate acquire — another thread may have
        // already done the fsync we wanted.
        if self.synced_lsn.load(Ordering::Acquire) >= lsn.0 {
            return Ok(());
        }

        // Direct-IO mode: flush any partially buffered records
        // (records that haven't reached a buffer-full boundary)
        // through a sector-aligned positioned write *before* the
        // fsync. This is what makes group-commit work in direct
        // mode — the partial-flush write puts the in-memory
        // records on the device's write queue; the subsequent
        // fsync moves them through to durable storage.
        if let Some(buffer_mutex) = &self.log_buffer {
            let mut buf = buffer_mutex.lock().unwrap_or_else(|p| p.into_inner());
            buf.flush_partial(&self.file)?;
        }

        // Capture the current append frontier. We'll mark
        // synced_lsn = frontier after fsync. Data appended AFTER
        // we capture this frontier may or may not be flushed by
        // the kernel — we conservatively report only up to the
        // captured frontier as synced.
        let frontier = self.next_lsn.load(Ordering::Acquire);

        // fsync. We use sync_data (fdatasync on Linux) — the
        // file size is the metadata we care about for resume,
        // and append-only writes don't change directory entries
        // or other metadata. `sync_data` takes `&self`, so no
        // mutex needed; concurrent appenders may still be running
        // (their writes may or may not be flushed by this call,
        // which is the documented group-commit semantics — we
        // only guarantee `frontier` is on disk after this call).
        self.file.sync_data().map_err(Error::Io)?;

        self.synced_lsn.store(frontier, Ordering::Release);
        #[cfg(feature = "tracing")]
        tracing::debug!(new_synced_lsn = frontier, "group-commit fsync completed");
        Ok(())
    }

    /// Returns the highest LSN currently known to be on stable
    /// storage.
    ///
    /// Increases monotonically as [`Self::sync_through`] calls
    /// complete. Useful for observability — e.g. exposing
    /// "durable bytes written" as a metric.
    #[must_use]
    pub fn synced_lsn(&self) -> Lsn {
        Lsn(self.synced_lsn.load(Ordering::Acquire))
    }

    /// Returns the next LSN that would be assigned by
    /// [`Self::append`] — i.e. the current end-of-journal cursor.
    ///
    /// Useful for snapshotting / replication: `next_lsn()` at a
    /// point in time tells you "everything appended up to here."
    #[must_use]
    pub fn next_lsn(&self) -> Lsn {
        Lsn(self.next_lsn.load(Ordering::Acquire))
    }

    /// Pre-allocates `len` bytes of disk space for this journal
    /// starting at `offset`. Reserves filesystem extents up-front
    /// so subsequent appends don't trigger allocation in the IO
    /// hot path. Critical for high-throughput WAL workloads
    /// where allocation jitter creates long-tail latency.
    ///
    /// Typical usage: call once after [`crate::Handle::journal`]
    /// returns, passing the expected total journal size (or a
    /// generous upper bound). The journal can then sustain writes
    /// without the filesystem allocating-on-write.
    ///
    /// `offset = 0` means "start at the beginning"; `len` is
    /// the number of bytes to reserve.
    ///
    /// # Platform behaviour
    ///
    /// - **Linux:** `fallocate(FALLOC_FL_KEEP_SIZE)` — reserves
    ///   extents without writing zeros. Falls back to
    ///   `posix_fallocate` (writes zeros) on filesystems that
    ///   don't support `fallocate`.
    /// - **macOS:** `fcntl(F_PREALLOCATE)` with contiguous
    ///   allocation; falls back to non-contiguous.
    /// - **Windows:** `SetEndOfFile` — bounds the logical size
    ///   so NTFS plans extents. True physical preallocation
    ///   (zeroing every block) requires admin privileges.
    /// - **Other platforms:** no-op (succeeds; allocation
    ///   happens on write).
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] on the underlying syscall failure.
    pub fn preallocate(&self, offset: u64, len: u64) -> Result<()> {
        crate::platform::preallocate(&self.file, offset, len)
    }

    /// Hints the kernel about the access pattern for a region
    /// of this journal. The kernel uses the hint to drive
    /// page-cache prefetch / eviction / read-ahead.
    ///
    /// Hints are advisory — never affects correctness, only
    /// performance. See [`crate::Advice`] for the available
    /// hint variants.
    ///
    /// `len = 0` means "the rest of the file from `offset`."
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] on the underlying syscall failure
    ///   (rare; most platforms return success even when the
    ///   hint isn't actually honoured).
    pub fn advise(&self, offset: u64, len: u64, advice: crate::Advice) -> Result<()> {
        crate::platform::advise(&self.file, offset, len, advice)
    }

    /// Performs a final sync and consumes the handle.
    ///
    /// Equivalent to `self.sync_through(self.next_lsn())` plus
    /// closing the file. Use when you want explicit
    /// success/failure reporting on the close path; otherwise,
    /// just drop the handle (the implicit close is best-effort).
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] on fsync or close failure.
    pub fn close(self) -> Result<()> {
        let frontier = self.next_lsn.load(Ordering::Acquire);
        self.sync_through(Lsn(frontier))?;
        // File closes when `self` drops; explicit drop here for
        // documentation.
        drop(self);
        Ok(())
    }
}

// JournalHandle is Send + Sync (Mutex<File>, AtomicU64s, PathBuf
// are all Send + Sync). Compile-time-asserted by the next two
// lines so a future field with !Send/!Sync state is caught by
// the type system.
#[allow(dead_code)]
fn _assert_journal_handle_is_send() {
    fn require_send<T: Send>(_: &T) {}
    fn require_sync<T: Sync>(_: &T) {}
    let _ = |h: &JournalHandle| {
        require_send(h);
        require_sync(h);
    };
}

impl Drop for JournalHandle {
    fn drop(&mut self) {
        // Direct-mode best-effort flush. The user-facing
        // [`Self::close`] path is preferred (it returns errors);
        // Drop is the safety-net for handles dropped without
        // close — flush whatever's in the log buffer so the
        // partial trailing sector lands on disk before we lose
        // the writer's view of it.
        if let Some(buffer_mutex) = &self.log_buffer {
            if let Ok(mut buf) = buffer_mutex.lock() {
                let _ = buf.flush_partial(&self.file);
            }
            let _ = self.file.sync_data();
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Internal helpers — direct-mode constructor + resume-scan
// ─────────────────────────────────────────────────────────────────

/// Scans `path` for the byte offset immediately past the last
/// cleanly-decoded frame. Used by direct-mode resume to set
/// `next_lsn` past partial / corrupted trailing bytes rather than
/// at raw `file_size`.
///
/// Returns `0` for an empty / non-existent file. Surfaces an error
/// for non-recoverable tail states (`BadMagic`, `LengthOverflow`)
/// so the caller can choose to refuse the open rather than
/// silently truncate past suspect data.
fn scan_clean_end(path: &Path) -> Result<u64> {
    let mut reader = JournalReader::open(path)?;
    if reader.file_size() == 0 {
        return Ok(0);
    }
    let mut iter = reader.iter();
    while iter.next().transpose()?.is_some() {}
    drop(iter);
    match reader.tail_state() {
        JournalTailState::CleanEnd
        | JournalTailState::TruncatedHeader
        | JournalTailState::TruncatedPayload
        | JournalTailState::ChecksumMismatch => Ok(reader.position().0),
        JournalTailState::BadMagic => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("journal at {:?} has bad magic at offset {} — refusing to open in direct mode", path, reader.position().0),
        ))),
        JournalTailState::LengthOverflow => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("journal at {:?} has frame length overflow at offset {} — refusing to open in direct mode", path, reader.position().0),
        ))),
    }
}

/// Open the journal file with the platform's Direct-IO flag.
/// Returns `(file, direct_active)`. `direct_active = false` means
/// the filesystem rejected `O_DIRECT` and the caller should fall
/// back to buffered semantics.
#[cfg(target_os = "linux")]
fn open_direct_journal(path: &Path, _sector_size: u32) -> Result<(File, bool)> {
    use std::os::fd::FromRawFd;
    let path_cstr =
        std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()).map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "journal path contains a NUL byte",
            ))
        })?;
    let mut flags = libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_DIRECT;
    // SAFETY: path_cstr is a valid NUL-terminated string; flags +
    // mode are valid open(2) arguments.
    let fd = unsafe { libc::open(path_cstr.as_ptr(), flags, 0o600_i32) };
    if fd >= 0 {
        // SAFETY: fd is a valid open file descriptor we just
        // created/opened.
        return Ok((unsafe { File::from_raw_fd(fd) }, true));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EINVAL) {
        // O_DIRECT rejected (tmpfs, FUSE, certain CIFS mounts).
        // Retry without it; the caller falls back to buffered.
        flags &= !libc::O_DIRECT;
        // SAFETY: same as above.
        let fd2 = unsafe { libc::open(path_cstr.as_ptr(), flags, 0o600_i32) };
        if fd2 >= 0 {
            // SAFETY: fd2 is a valid open file descriptor.
            return Ok((unsafe { File::from_raw_fd(fd2) }, false));
        }
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Err(Error::Io(err))
}

#[cfg(target_os = "macos")]
fn open_direct_journal(path: &Path, _sector_size: u32) -> Result<(File, bool)> {
    use std::os::unix::io::AsRawFd;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(Error::Io)?;
    // Set F_NOCACHE — macOS's analogue of O_DIRECT. SAFETY: the
    // fd is owned by `file`, which lives across this call.
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
    Ok((file, ret == 0))
}

#[cfg(target_os = "windows")]
fn open_direct_journal(path: &Path, _sector_size: u32) -> Result<(File, bool)> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_SHARE_READ, OPEN_ALWAYS,
    };

    // Convert path to wide string with trailing NUL.
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    // First attempt: with FILE_FLAG_NO_BUFFERING + FILE_FLAG_WRITE_THROUGH.
    // SAFETY: wide is a valid wide-encoded path with trailing NUL.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
            std::ptr::null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
        // SAFETY: handle is a valid HANDLE we just opened.
        return Ok((unsafe { File::from_raw_handle(handle as _) }, true));
    }

    // SAFETY: GetLastError is a thread-local Win32 query with no
    // pre-conditions; safe to call from any thread.
    let err_code = unsafe { GetLastError() };
    // Some Windows filesystems / network shares reject
    // FILE_FLAG_NO_BUFFERING (ERROR_INVALID_PARAMETER == 87). Fall
    // back to a standard buffered open.
    if err_code == 87 {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(Error::Io)?;
        return Ok((file, false));
    }
    Err(Error::Io(std::io::Error::from_raw_os_error(
        err_code as i32,
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn open_direct_journal(path: &Path, _sector_size: u32) -> Result<(File, bool)> {
    // No Direct-IO on unknown platforms — fall back silently.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(Error::Io)?;
    Ok((file, false))
}

/// Rehydrate the log buffer's first sector from the on-disk
/// content of the partial trailing sector. Used on resume so that
/// subsequent flushes overwrite the zero-pad cleanly without
/// destroying records.
fn rehydrate_log_buffer(
    buf: &mut LogBuffer,
    file: &File,
    sector_size: u32,
    resume_lsn: u64,
) -> Result<()> {
    let ss = sector_size as u64;
    let last_sector_start = (resume_lsn / ss) * ss;
    let in_sector_offset = (resume_lsn - last_sector_start) as usize;
    if in_sector_offset == 0 {
        // resume_lsn lands exactly on a sector boundary; nothing
        // to rehydrate, the buffer is already initialised to
        // (flush_pos = 0, len = 0). Move flush_pos forward.
        buf.set_flush_pos_for_resume(resume_lsn, 0, &[]);
        return Ok(());
    }
    // Read the partial trailing sector from disk.
    let bytes = crate::platform::read_range(file, last_sector_start, sector_size as usize)?;
    buf.set_flush_pos_for_resume(last_sector_start, in_sector_offset, &bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsn_default_is_zero() {
        assert_eq!(Lsn::default(), Lsn::ZERO);
        assert_eq!(Lsn::ZERO.as_u64(), 0);
    }

    #[test]
    fn lsn_display_format() {
        assert_eq!(format!("{}", Lsn(42)), "Lsn(42)");
    }

    #[test]
    fn lsn_ordering_matches_u64() {
        assert!(Lsn(100) < Lsn(200));
        assert!(Lsn(0) < Lsn(1));
        assert_eq!(Lsn(42), Lsn(42));
    }

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "fsys_journal_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            tag
        ))
    }

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn open_creates_new_file_with_zero_lsn() {
        let path = tmp_path("new");
        let _g = Cleanup(path.clone());
        let j = JournalHandle::open(&path).expect("open");
        assert_eq!(j.next_lsn(), Lsn::ZERO);
        assert_eq!(j.synced_lsn(), Lsn::ZERO);
    }

    #[test]
    fn append_advances_lsn_by_framed_record_length() {
        // Each record is wrapped in a 12-byte frame
        // (magic + length + crc32c). LSN advances by
        // frame.len() = payload.len() + 12.
        let path = tmp_path("append");
        let _g = Cleanup(path.clone());
        let j = JournalHandle::open(&path).expect("open");

        let lsn1 = j.append(b"hello").expect("append1");
        assert_eq!(lsn1, Lsn(5 + 12));
        assert_eq!(j.next_lsn(), Lsn(17));

        let lsn2 = j.append(b" world").expect("append2");
        assert_eq!(lsn2, Lsn(17 + 6 + 12));
        assert_eq!(j.next_lsn(), Lsn(35));
    }

    #[test]
    fn append_empty_record_writes_framed_marker() {
        // Empty records are valid — they produce a 12-byte
        // header-only frame (length=0, crc over magic+length).
        // Useful for marking checkpoints / transaction
        // boundaries in the journal stream.
        let path = tmp_path("empty_record");
        let _g = Cleanup(path.clone());
        let j = JournalHandle::open(&path).expect("open");
        let _ = j.append(b"first").expect("first");
        assert_eq!(j.next_lsn(), Lsn(5 + 12));
        let lsn = j.append(b"").expect("empty");
        assert_eq!(lsn, Lsn(17 + 12));
        assert_eq!(j.next_lsn(), Lsn(29));
    }

    #[test]
    fn sync_through_zero_is_noop() {
        let path = tmp_path("sync_zero");
        let _g = Cleanup(path.clone());
        let j = JournalHandle::open(&path).expect("open");
        j.sync_through(Lsn::ZERO).expect("sync_through(0)");
        assert_eq!(j.synced_lsn(), Lsn::ZERO);
    }

    #[test]
    fn sync_through_advances_synced_lsn() {
        let path = tmp_path("sync_advance");
        let _g = Cleanup(path.clone());
        let j = JournalHandle::open(&path).expect("open");
        let lsn = j.append(b"durable").expect("append");
        j.sync_through(lsn).expect("sync");
        assert!(j.synced_lsn() >= lsn);
    }

    #[test]
    fn append_then_reopen_resumes_at_existing_size() {
        let path = tmp_path("resume");
        let _g = Cleanup(path.clone());
        {
            let j = JournalHandle::open(&path).expect("open1");
            let _ = j.append(b"persist this").expect("append");
            j.close().expect("close");
        }
        let j2 = JournalHandle::open(&path).expect("reopen");
        // 12-byte payload + 12-byte frame overhead = 24 bytes.
        assert_eq!(j2.next_lsn(), Lsn(24));
        assert_eq!(j2.synced_lsn(), Lsn(24));
    }

    #[test]
    fn append_writes_framed_records_to_file() {
        // Verify the on-disk format: each record is wrapped in
        // its frame. We confirm the file size matches the sum
        // of (payload + FRAME_OVERHEAD) per record, and that
        // the frame can be decoded back to the original payload.
        let path = tmp_path("readback");
        let _g = Cleanup(path.clone());
        let j = JournalHandle::open(&path).expect("open");
        let _ = j.append(b"alpha").expect("a1");
        let _ = j.append(b"beta").expect("a2");
        j.close().expect("close");
        let bytes = std::fs::read(&path).expect("read");

        // Total: (5 + 12) + (4 + 12) = 33 bytes.
        assert_eq!(bytes.len(), 33);

        // Decode frame 1.
        match format::decode_frame(&bytes) {
            format::FrameDecode::Ok {
                consumed,
                payload_start,
                payload_end,
            } => {
                assert_eq!(consumed, 17);
                assert_eq!(&bytes[payload_start..payload_end], b"alpha");

                // Decode frame 2 starting at offset 17.
                match format::decode_frame(&bytes[17..]) {
                    format::FrameDecode::Ok {
                        consumed,
                        payload_start,
                        payload_end,
                    } => {
                        assert_eq!(consumed, 16);
                        assert_eq!(&bytes[17 + payload_start..17 + payload_end], b"beta");
                    }
                    other => panic!("frame 2 decode failed: {other:?}"),
                }
            }
            other => panic!("frame 1 decode failed: {other:?}"),
        }
    }

    #[test]
    fn group_commit_concurrent_sync_through() {
        use std::sync::Arc;
        let path = tmp_path("group_commit");
        let _g = Cleanup(path.clone());
        let j = Arc::new(JournalHandle::open(&path).expect("open"));
        let mut lsns = Vec::new();
        for i in 0..32 {
            let lsn = j.append(format!("rec {i:04}").as_bytes()).expect("append");
            lsns.push(lsn);
        }
        // Concurrent sync_through from many threads — should
        // coalesce into one fsync.
        let mut handles = Vec::new();
        for lsn in &lsns {
            let j = j.clone();
            let lsn = *lsn;
            handles.push(std::thread::spawn(move || {
                j.sync_through(lsn).expect("sync_through")
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        assert!(j.synced_lsn() >= *lsns.last().unwrap());
    }
}
