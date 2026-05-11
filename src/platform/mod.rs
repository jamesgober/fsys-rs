//! Per-platform IO primitives.
//!
//! This module provides a uniform `pub(crate)` interface over the
//! platform-specific IO operations that fsys needs: direct-IO file opening,
//! positioned read/write, durability flushes, atomic rename, parent-directory
//! sync, optimised file copy, and sector-size probing.
//!
//! Each submodule implements the same set of functions for its target OS.
//! The active submodule is aliased to `imp` and its symbols are re-exported
//! from this module so callers can write `crate::platform::open_write_new(…)`
//! without knowing which platform is active.
//!
//! # Platform-specific behavior
//!
//! - **Linux** (`platform/linux.rs`): `O_DIRECT`, `pwrite`/`pread`,
//!   `fdatasync`, `fsync`, `copy_file_range`, `renameat2`-with-fallback.
//! - **macOS** (`platform/macos.rs`): `F_NOCACHE`, `pwrite`/`pread`,
//!   `F_FULLFSYNC`, `clonefile`-with-fallback.
//! - **Windows** (`platform/windows.rs`): `CreateFileW` with
//!   `FILE_FLAG_NO_BUFFERING|WRITE_THROUGH`, `ReadFile`/`WriteFile`,
//!   `FlushFileBuffers`, `MoveFileExW`.
//! - **Unknown** (`platform/unknown.rs`): pure `std::fs` fallback.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

// io_uring wrapper — Linux only. Lazy-init per-Handle ring used by
// `Method::Direct`'s elite path (locked decision #1 in
// `.dev/DECISIONS-0.5.0.md`). Falls back to `pwrite` + `fdatasync`
// when `io_uring_setup(2)` is unavailable.
#[cfg(target_os = "linux")]
pub(crate) mod linux_iouring;

// io_uring kernel-feature probe — Linux only. Runs a single
// process-wide probe (cached via OnceLock) for the elite setup
// flags COOP_TASKRUN / SINGLE_ISSUER / DEFER_TASKRUN, then
// applies the supported subset to every ring built by the
// crate. New in 0.9.4.
#[cfg(target_os = "linux")]
pub(crate) mod iouring_features;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as imp;

// Windows NVMe passthrough flush via `IOCTL_STORAGE_PROTOCOL_COMMAND`
// (locked decision D-2 in `.dev/DECISIONS-0.6.0.md`). Capability
// detection at first Direct op; falls back to
// `FILE_FLAG_WRITE_THROUGH` when the IOCTL is unavailable.
#[cfg(target_os = "windows")]
pub(crate) mod windows_nvme;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unknown;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use unknown as imp;

// ──────────────────────────────────────────────────────────────────────────────
// Aligned-buffer utility for Direct IO.
//
// Direct IO requires that buffer address, length, and file offset are all
// multiples of the logical sector size (typically 512 or 4096 bytes).
// When the caller's data does not meet these requirements, fsys allocates a
// heap-aligned scratch buffer, copies the data in, and writes from the aligned
// address.
//
// Allocation cost: one `alloc + dealloc` per Direct IO operation on
// unaligned input. The 64 KiB stack-buffer optimisation is deferred to
// 0.5.0; all Direct IO alignment uses heap allocation in 0.3.0.
// ──────────────────────────────────────────────────────────────────────────────

use std::alloc::{self, Layout};
use std::ptr::NonNull;

/// Heap-allocated buffer with a guaranteed minimum alignment.
///
/// Wraps a raw allocation so the memory is freed on drop even if the
/// operation fails part-way through.
pub(crate) struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
    /// Usable length of the allocation (may be larger than requested due to
    /// sector-size rounding).
    pub(crate) len: usize,
}

impl AlignedBuf {
    /// Allocates `size` bytes aligned to `align` bytes, zero-initialised.
    ///
    /// Returns an error if `align` is not a power of two, if `size` is
    /// zero, or if the allocator returns null. Zero size is rejected
    /// because `alloc_zeroed` requires `layout.size() > 0` — call sites
    /// must short-circuit empty input before reaching this function.
    pub(crate) fn new(size: usize, align: usize) -> crate::Result<Self> {
        if size == 0 {
            return Err(crate::Error::AlignmentRequired {
                detail: "AlignedBuf::new called with size=0; callers must short-circuit empty Direct IO before reaching the buffer allocator",
            });
        }
        let layout =
            Layout::from_size_align(size, align).map_err(|_| crate::Error::AlignmentRequired {
                detail: "invalid size/align combination for Direct IO buffer",
            })?;
        // SAFETY: layout.size() > 0 enforced by the guard above; align
        // is a power of two enforced by Layout::from_size_align.
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or(crate::Error::Io(std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            "Direct IO aligned buffer allocation failed",
        )))?;
        Ok(AlignedBuf {
            ptr,
            layout,
            len: size,
        })
    }

    /// Returns a shared slice of the allocation.
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is valid, non-null, and len ≤ layout.size().
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Returns a mutable slice of the allocation.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid, non-null, mutable, and len ≤ layout.size().
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: ptr was returned by alloc_zeroed with this exact layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

// SAFETY: `AlignedBuf` owns its allocation exclusively (the
// pointer is never duplicated; `Drop` is the only deallocator),
// so transferring ownership across threads is sound — same
// reasoning as `Vec<u8>`. `NonNull<u8>` is `!Send + !Sync` by
// default only because it might in general represent an aliased
// pointer; here it does not.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared `&AlignedBuf` access is read-only via
// `as_slice`, which is the same access shape as `&[u8]`. No
// interior mutability is possible.
unsafe impl Sync for AlignedBuf {}

/// Rounds `n` up to the next multiple of `align`.
///
/// `align` must be a power of two and non-zero.
#[inline]
pub(crate) fn round_up(n: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two(), "align must be a power of two");
    (n + align - 1) & !(align - 1)
}

// ──────────────────────────────────────────────────────────────────────────────
// Public(crate) cross-platform API — delegates to the active platform module.
// ──────────────────────────────────────────────────────────────────────────────

/// Opens `path` for writing as a new (must-not-exist) file.
///
/// Returns the file and a flag indicating whether Direct IO was actually
/// activated. When `use_direct` is `true` but the filesystem rejects it
/// (e.g. tmpfs on Linux), the file is re-opened without Direct IO and the
/// returned flag is `false`.
///
/// # Platform-specific behavior
///
/// - Linux: `O_WRONLY|O_CREAT|O_EXCL`, optionally `|O_DIRECT`.
/// - macOS: standard create-new open, then `fcntl(F_NOCACHE, 1)` when
///   `use_direct` is true.
/// - Windows: `CreateFileW(CREATE_NEW)`, optionally with
///   `FILE_FLAG_NO_BUFFERING|FILE_FLAG_WRITE_THROUGH`.
/// - Unknown: `std::fs::OpenOptions` create-new.
#[inline]
pub(crate) fn open_write_new(
    path: &std::path::Path,
    use_direct: bool,
) -> crate::Result<(std::fs::File, bool)> {
    imp::open_write_new(path, use_direct)
}

/// Opens `path` for reading.
///
/// Returns the file and a flag indicating whether Direct IO is active.
///
/// # Platform-specific behavior
///
/// Same Direct IO semantics as [`open_write_new`], but with read-only access.
#[inline]
pub(crate) fn open_read(
    path: &std::path::Path,
    use_direct: bool,
) -> crate::Result<(std::fs::File, bool)> {
    imp::open_read(path, use_direct)
}

/// Opens `path` for appending (creates if missing).
///
/// Always uses standard (non-Direct) IO. `O_APPEND` / `FILE_APPEND_DATA`
/// ensures OS-level atomicity for writes up to `PIPE_BUF` bytes on POSIX.
#[inline]
pub(crate) fn open_append(path: &std::path::Path) -> crate::Result<std::fs::File> {
    imp::open_append(path)
}

/// Opens `path` for random-access writing (existing file, no truncation).
///
/// Used by [`crate::Handle::write_at`]. Direct IO is **not** used here
/// because arbitrary offsets would require a costly read-modify-write cycle
/// on every unaligned access. See the `write_at` doc comment for details.
#[inline]
pub(crate) fn open_write_at(path: &std::path::Path) -> crate::Result<std::fs::File> {
    imp::open_write_at(path)
}

/// Writes `data` to `file` using standard (buffered) IO.
///
/// Used when Direct IO is not active.
#[inline]
pub(crate) fn write_all(file: &std::fs::File, data: &[u8]) -> crate::Result<()> {
    imp::write_all(file, data)
}

/// Writes `data` to `file` using Direct IO, with internal alignment handling.
///
/// If `data` length is not a multiple of `sector_size`, the remainder is
/// zero-padded in an aligned scratch buffer before the write. This matches
/// the kernel's requirement that every `O_DIRECT` write is sector-aligned.
///
/// # Platform-specific behavior
///
/// - Linux: `pwrite(2)` with an aligned buffer; offset 0.
/// - macOS: standard `write(2)` on an `F_NOCACHE` fd; alignment handled by
///   zero-padding to sector boundary.
/// - Windows: `WriteFile` through a `FILE_FLAG_NO_BUFFERING` handle with an
///   aligned buffer.
/// - Unknown: delegates to [`write_all`] (no Direct IO on unknown platforms).
#[inline]
pub(crate) fn write_all_direct(
    file: &std::fs::File,
    data: &[u8],
    sector_size: u32,
) -> crate::Result<()> {
    imp::write_all_direct(file, data, sector_size)
}

/// Writes `data` to `file` at `offset` bytes using standard IO.
///
/// Uses `pwrite(2)` on Unix and `SetFilePointerEx` + `WriteFile` on
/// Windows. This is **not** crash-atomic — a power failure mid-write
/// may leave the file in a partially updated state. Callers that need
/// crash safety should use [`crate::Handle::write`] instead.
#[inline]
pub(crate) fn write_at(file: &std::fs::File, offset: u64, data: &[u8]) -> crate::Result<()> {
    imp::write_at(file, offset, data)
}

/// Sector-aligned positioned write for Direct IO file handles.
///
/// **Pre-conditions** (caller-enforced — not validated here on the
/// hot path; violations surface as kernel `EINVAL`):
/// - `data.as_ptr()` is sector-aligned.
/// - `data.len()` is a multiple of the underlying device's sector size.
/// - `offset` is a multiple of the sector size.
///
/// Used by the direct-IO journal log buffer (`JournalOptions::direct(true)`),
/// which owns an `AlignedBuf` and flushes only at sector boundaries.
#[inline]
pub(crate) fn write_at_direct(file: &std::fs::File, offset: u64, data: &[u8]) -> crate::Result<()> {
    imp::write_at_direct(file, offset, data)
}

/// Reads the entire content of `file` into a `Vec<u8>`.
#[inline]
pub(crate) fn read_all(file: &std::fs::File) -> crate::Result<Vec<u8>> {
    imp::read_all(file)
}

/// Reads the entire content of `file` into a `Vec<u8>` using Direct IO.
///
/// Allocates an aligned buffer of `file_size` rounded up to the next
/// sector boundary, reads, then trims to `file_size`.
#[inline]
pub(crate) fn read_all_direct(
    file: &std::fs::File,
    file_size: u64,
    sector_size: u32,
) -> crate::Result<Vec<u8>> {
    imp::read_all_direct(file, file_size, sector_size)
}

/// Reads `len` bytes from `file` starting at `offset`.
#[inline]
pub(crate) fn read_range(file: &std::fs::File, offset: u64, len: usize) -> crate::Result<Vec<u8>> {
    imp::read_range(file, offset, len)
}

/// Flushes data-only (equivalent of `fdatasync`).
///
/// On platforms without `fdatasync` (macOS, Windows), falls back to a
/// full flush. The caller is responsible for updating `active_method()`
/// when this fallback occurs.
#[inline]
pub(crate) fn sync_data(file: &std::fs::File) -> crate::Result<()> {
    imp::sync_data(file)
}

/// Full file flush (equivalent of `fsync` / `F_FULLFSYNC`).
#[inline]
pub(crate) fn sync_full(file: &std::fs::File) -> crate::Result<()> {
    imp::sync_full(file)
}

/// 0.9.4 — Sets the per-file NVMe write-lifetime hint
/// (`F_SET_RW_HINT` on Linux).
///
/// `hint_ordinal` is the 0-based discriminant of
/// [`crate::WriteLifetimeHint`]:
/// `0 = Short`, `1 = Medium`, `2 = Long`, `3 = Extreme`.
///
/// **Platforms:**
/// - **Linux**: applies the `F_SET_RW_HINT` fcntl. Failure
///   (older kernels, drives without multi-stream, FS rejection)
///   returns `Err` — the journal-open path swallows the error
///   because the hint is advisory.
/// - **macOS / Windows / unknown**: silent no-op. The hint
///   primitive doesn't exist; returning `Ok(())` is the honest
///   answer (we successfully did nothing).
#[inline]
#[cfg_attr(
    not(target_os = "linux"),
    allow(unused_variables, clippy::needless_pass_by_value)
)]
pub(crate) fn set_write_lifetime_hint(file: &std::fs::File, hint_ordinal: u8) -> crate::Result<()> {
    #[cfg(target_os = "linux")]
    {
        imp::fcntl_set_rw_hint(file, hint_ordinal)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
}

/// 0.9.4 — Barrier-grade sync. Cheaper than [`sync_full`]
/// where the platform supports it.
///
/// **Platform mapping:**
/// - **macOS:** `fcntl(F_BARRIERFSYNC)` — ordering guarantee
///   without forcing the drive to flush its write cache to
///   media. Crash-safe **only** on drives with PLP (or when
///   paired with an eventual `sync_full` at a commit
///   boundary). Dramatically cheaper than `F_FULLFSYNC` on
///   Apple Silicon NVMe.
/// - **Linux:** `fdatasync(2)` — already barrier-grade by
///   default; same as `sync_data`.
/// - **Windows:** no-op. `FILE_FLAG_WRITE_THROUGH` already
///   provides durable-on-return semantics for every write;
///   there is no separate barrier primitive to call.
/// - **Unknown:** falls back to `sync_data`.
///
/// **Used internally** by [`crate::JournalHandle::sync_through`]
/// when the journal was opened with
/// `JournalOptions::sync_mode(SyncMode::Barrier)`. The default
/// `SyncMode::Full` retains the pre-0.9.4 behaviour (every
/// `sync_through` calls `sync_data` → `fsync`/`F_FULLFSYNC`).
#[inline]
pub(crate) fn sync_barrier(file: &std::fs::File) -> crate::Result<()> {
    #[cfg(target_os = "macos")]
    {
        imp::sync_barrier(file)
    }
    #[cfg(target_os = "linux")]
    {
        // fdatasync IS the barrier-grade primitive on Linux.
        imp::sync_data(file)
    }
    #[cfg(target_os = "windows")]
    {
        // WRITE_THROUGH already made every write durable on
        // return; the per-handle file has nothing pending.
        let _ = file;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        imp::sync_data(file)
    }
}

/// Atomically renames `from` to `to`, replacing `to` if it exists.
///
/// # Platform-specific behavior
///
/// - Unix: POSIX `rename(2)`, which is atomic within the same filesystem.
/// - Windows: `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING |
///   MOVEFILE_WRITE_THROUGH`.
#[inline]
pub(crate) fn atomic_rename(from: &std::path::Path, to: &std::path::Path) -> crate::Result<()> {
    imp::atomic_rename(from, to)
}

/// Opens the parent directory and calls `fsync` on it.
///
/// Required on Linux and macOS after an atomic rename to guarantee that the
/// directory entry update is durable. No-op on Windows (directory durability
/// is implicit with `WRITE_THROUGH`) and on unknown platforms.
#[inline]
pub(crate) fn sync_parent_dir(path: &std::path::Path) -> crate::Result<()> {
    imp::sync_parent_dir(path)
}

/// Copies `src` to `dst` using the best available platform primitive.
///
/// # Platform-specific behavior
///
/// - Linux: `copy_file_range(2)` for same-filesystem copies; `std::fs::copy`
///   fallback.
/// - macOS: `clonefile(2)` when available; `std::fs::copy` fallback.
/// - Windows/Unknown: `std::fs::copy`.
#[inline]
pub(crate) fn copy_file(src: &std::path::Path, dst: &std::path::Path) -> crate::Result<u64> {
    imp::copy_file(src, dst)
}

/// Probes the logical sector / block size for the filesystem hosting `path`.
///
/// Returns a conservative default of `512` when the probe is unavailable.
/// The sector size is used to set up aligned scratch buffers for Direct IO.
#[inline]
pub(crate) fn probe_sector_size(path: &std::path::Path) -> u32 {
    imp::probe_sector_size(path)
}

/// Returns `true` when Direct IO is potentially available on this platform.
///
/// A `true` result means the kernel-level API exists; actual availability
/// depends on the filesystem and is confirmed at file-open time.
#[allow(dead_code)]
#[inline]
pub(crate) fn probe_direct_io_available() -> bool {
    imp::probe_direct_io_available()
}

// ─────────────────────────────────────────────────────────────────────────
// Storage-engine primitives — extent preallocation + access-pattern hints
// ─────────────────────────────────────────────────────────────────────────

/// Pre-allocates `len` bytes of disk space for `file` starting at
/// `offset`. Reserves filesystem extents up-front so subsequent writes
/// don't trigger allocation in the IO hot path. Critical for
/// high-throughput WAL workloads where allocation jitter creates
/// long-tail latency.
///
/// # Platform-specific behavior
///
/// - **Linux:** `fallocate(fd, FALLOC_FL_KEEP_SIZE, offset, len)` —
///   reserves extents without changing the logical file size. The
///   journal can then write into the pre-allocated region knowing the
///   filesystem won't need to allocate blocks mid-write. On
///   filesystems that don't support fallocate (some FUSE, network),
///   falls back to `posix_fallocate` which writes zeros.
/// - **macOS:** `fcntl(fd, F_PREALLOCATE, ...)` with
///   `F_ALLOCATECONTIG | F_ALLOCATEALL` flags. Falls back to
///   `F_ALLOCATEALL` alone if contiguous allocation fails.
/// - **Windows:** `SetEndOfFile` to extend the logical size. True
///   physical preallocation requires `SetFileValidData` which
///   needs the `SE_MANAGE_VOLUME_NAME` privilege; we use it only
///   when the privilege is detected (caller running as
///   administrator). Without the privilege the kernel allocates
///   on the first write — same as not calling preallocate.
/// - **Unknown:** no-op (succeeds; the OS allocates on write).
///
/// # Errors
///
/// - [`Error::Io`](crate::Error::Io) on the underlying syscall failure.
#[allow(dead_code)]
#[inline]
pub(crate) fn preallocate(file: &std::fs::File, offset: u64, len: u64) -> crate::Result<()> {
    imp::preallocate(file, offset, len)
}

/// Hints the kernel about how `file` will be accessed in the
/// `[offset, offset+len)` byte range. The kernel uses these hints
/// to drive page-cache pre-fetch, eviction, and read-ahead policy.
///
/// `len = 0` means "the rest of the file from `offset` onward."
///
/// See [`Advice`] for the available hint variants.
#[allow(dead_code)]
#[inline]
pub(crate) fn advise(
    file: &std::fs::File,
    offset: u64,
    len: u64,
    advice: crate::Advice,
) -> crate::Result<()> {
    imp::advise(file, offset, len, advice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_up_no_op_when_aligned() {
        assert_eq!(round_up(512, 512), 512);
        assert_eq!(round_up(4096, 512), 4096);
    }

    #[test]
    fn test_round_up_pads_to_next_boundary() {
        assert_eq!(round_up(1, 512), 512);
        assert_eq!(round_up(513, 512), 1024);
        assert_eq!(round_up(4097, 4096), 8192);
    }

    #[test]
    fn test_round_up_zero_returns_zero() {
        assert_eq!(round_up(0, 512), 0);
    }

    #[test]
    fn test_aligned_buf_creates_and_drops_cleanly() {
        let buf = AlignedBuf::new(4096, 512).expect("alloc aligned buf");
        assert_eq!(buf.len, 4096);
        assert!(buf.as_slice().iter().all(|&b| b == 0), "must be zero-init");
    }

    #[test]
    fn test_aligned_buf_write_and_read() {
        let mut buf = AlignedBuf::new(512, 512).expect("alloc");
        buf.as_mut_slice()[0] = 0xAB;
        assert_eq!(buf.as_slice()[0], 0xAB);
    }

    #[test]
    fn test_probe_sector_size_returns_nonzero() {
        let path = std::env::temp_dir();
        let size = probe_sector_size(&path);
        assert!(
            size >= 512,
            "sector size must be at least 512, got {}",
            size
        );
    }

    #[test]
    fn test_probe_direct_io_available_returns_bool() {
        // Just check it compiles and doesn't panic.
        let _available = probe_direct_io_available();
    }
}
