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
    /// Returns an error if `align` is not a power of two, if the size
    /// overflows, or if the allocator returns null.
    pub(crate) fn new(size: usize, align: usize) -> crate::Result<Self> {
        let layout =
            Layout::from_size_align(size, align).map_err(|_| crate::Error::AlignmentRequired {
                detail: "invalid size/align combination for Direct IO buffer",
            })?;
        // SAFETY: layout has non-zero size (Direct IO always writes ≥ 1 sector).
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
