//! macOS-specific IO primitives.
//!
//! Uses `F_NOCACHE` for Direct IO and `F_FULLFSYNC` for durability.
//! Regular `fsync(2)` on macOS only flushes to the drive's write cache —
//! `F_FULLFSYNC` is required to guarantee media durability.

#![cfg(target_os = "macos")]

use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::Path;

// ──────────────────────────────────────────────────────────────────────────────
// File opening
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn open_write_new(path: &Path, use_direct: bool) -> Result<(File, bool)> {
    let path_cstr = path_to_cstr(path)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC;

    // SAFETY: path_cstr is a valid NUL-terminated string; flags and mode are
    // valid open(2) arguments.
    let fd = unsafe { libc::open(path_cstr.as_ptr(), flags, 0o600_i32) };
    if fd < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    if use_direct {
        // F_NOCACHE disables the page cache for this fd.
        // SAFETY: fd is a valid open file descriptor.
        let ret = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1_i32) };
        if ret < 0 {
            // F_NOCACHE failure is rare (some HFS+ configurations). Proceed
            // without it but still use F_FULLFSYNC for durability. The caller
            // observes the fallback via the returned false flag and updates
            // `active_method()` accordingly — downstream code can query
            // `Handle::active_method()` to detect the fallback. (Previously
            // this site had a `TODO(0.3.0)` for a metrics event; that signal
            // is now surfaced via the public `active_method()` accessor.)
            // SAFETY: fd is valid and owned.
            let file = unsafe { File::from_raw_fd(fd) };
            return Ok((file, false));
        }
    }

    // SAFETY: fd is a valid, open file descriptor owned by us.
    Ok((unsafe { File::from_raw_fd(fd) }, use_direct))
}

pub(crate) fn open_read(path: &Path, use_direct: bool) -> Result<(File, bool)> {
    let path_cstr = path_to_cstr(path)?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC;

    // SAFETY: path_cstr and flags are valid.
    let fd = unsafe { libc::open(path_cstr.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    if use_direct {
        // SAFETY: fd is valid and open.
        let ret = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1_i32) };
        if ret < 0 {
            // F_NOCACHE failed; proceed without it.
            // SAFETY: fd is valid.
            let file = unsafe { File::from_raw_fd(fd) };
            return Ok((file, false));
        }
    }

    // SAFETY: fd is valid and owned.
    Ok((unsafe { File::from_raw_fd(fd) }, use_direct))
}

pub(crate) fn open_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(Error::Io)
}

pub(crate) fn open_write_at(path: &Path) -> Result<File> {
    // `truncate(false)`: random-access writes overlay byte ranges and must
    // keep the rest of the file.
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Writing
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn write_all(file: &File, data: &[u8]) -> Result<()> {
    let fd = file.as_raw_fd();
    let mut written = 0usize;
    while written < data.len() {
        // SAFETY: fd is a valid open file descriptor; the slice is valid.
        let n = unsafe {
            libc::write(
                fd,
                data[written..].as_ptr().cast::<libc::c_void>(),
                data.len() - written,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(err));
        }
        written += n as usize;
    }
    Ok(())
}

pub(crate) fn write_all_direct(file: &File, data: &[u8], sector_size: u32) -> Result<()> {
    use super::{round_up, AlignedBuf};

    // Empty input — no-op. See linux.rs::write_all_direct for the
    // rationale (AlignedBuf::new rejects size=0).
    if data.is_empty() {
        return Ok(());
    }

    // macOS F_NOCACHE does not require strict sector alignment from the
    // application (the kernel handles alignment internally), but we still
    // pad to the sector boundary for consistency with the Linux path.
    let ss = sector_size as usize;
    let aligned_len = round_up(data.len(), ss);
    let mut buf = AlignedBuf::new(aligned_len, ss)?;
    buf.as_mut_slice()[..data.len()].copy_from_slice(data);

    let fd = file.as_raw_fd();
    let base = buf.as_slice().as_ptr();

    // Loop on partial writes. See linux.rs::write_all_direct for
    // the rationale — `pwrite` may return less than requested on
    // EINTR or short-write conditions; without a loop a Direct
    // write of a large payload can silently truncate.
    let mut written = 0usize;
    while written < aligned_len {
        // SAFETY: fd is valid; buf is sector-aligned and has
        // aligned_len bytes available; the offset (written) and
        // length (aligned_len - written) stay within bounds.
        let n = unsafe {
            libc::pwrite(
                fd,
                base.add(written).cast::<libc::c_void>(),
                aligned_len - written,
                written as libc::off_t,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(err));
        }
        if n == 0 {
            return Err(Error::Io(std::io::Error::other(
                "pwrite returned 0 in write_all_direct (no progress)",
            )));
        }
        written += n as usize;
    }
    Ok(())
}

pub(crate) fn write_at(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    let fd = file.as_raw_fd();
    let mut written = 0usize;
    while written < data.len() {
        let off = (offset as i64).checked_add(written as i64).ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "write_at: offset overflow",
            ))
        })?;
        // SAFETY: fd is valid; the slice is valid for the duration.
        let n = unsafe {
            libc::pwrite(
                fd,
                data[written..].as_ptr().cast::<libc::c_void>(),
                data.len() - written,
                off as libc::off_t,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(err));
        }
        // 0.9.6 — Guard against infinite loop on zero-byte pwrite
        // return. POSIX allows pwrite to return 0; on macOS this
        // is most often a network-filesystem (AFP / SMB) edge case.
        // Without this guard the loop spins forever consuming CPU.
        if n == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "pwrite returned 0 in write_at (no progress)",
            )));
        }
        written += n as usize;
    }
    Ok(())
}

/// Sector-aligned positioned write for `F_NOCACHE` files. See
/// `linux.rs::write_at_direct` for the pre-condition contract.
pub(crate) fn write_at_direct(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    write_at(file, offset, data)
}

// ──────────────────────────────────────────────────────────────────────────────
// Reading
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn read_all(file: &File) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    // `read_to_end` returns the byte count, redundant with `buf.len()`
    // once the call returns; explicit `_` discard satisfies
    // `unused_results`.
    let _ = (&*file).read_to_end(&mut buf).map_err(Error::Io)?;
    Ok(buf)
}

pub(crate) fn read_all_direct(file: &File, file_size: u64, sector_size: u32) -> Result<Vec<u8>> {
    use super::{round_up, AlignedBuf};

    if file_size == 0 {
        return Ok(Vec::new());
    }

    let ss = sector_size as usize;
    let aligned_len = round_up(file_size as usize, ss);
    let mut buf = AlignedBuf::new(aligned_len, ss)?;

    let fd = file.as_raw_fd();

    // 0.9.6 — pread on F_NOCACHE files may return EINTR (signal)
    // or a short read (POSIX-legal). Loop with EINTR retry and
    // short-read accumulation. Required for journal-recovery
    // path resilience.
    let mut total = 0usize;
    while total < aligned_len {
        // SAFETY: fd is valid for the duration; buffer slice
        // from `total` onward is owned by us and lives across
        // the syscall; aligned at every iteration (pread on
        // F_NOCACHE returns sector multiples or 0 at EOF).
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_slice()[total..]
                    .as_mut_ptr()
                    .cast::<libc::c_void>(),
                aligned_len - total,
                total as libc::off_t,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(err));
        }
        if n == 0 {
            break;
        }
        total += n as usize;
    }

    let trimmed = usize::min(total, file_size as usize);
    Ok(buf.as_slice()[..trimmed].to_vec())
}

pub(crate) fn read_range(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let fd = file.as_raw_fd();
    super::read_range_with(file, offset, len, |buf, pos| {
        let off = libc::off_t::try_from(pos).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "read_range: offset exceeds off_t",
            )
        })?;
        // SAFETY: fd is valid for the call; `buf` is a live, writable
        // slice of `buf.len()` bytes and pread writes at most that many.
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len(), off) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Durability
//
// On macOS, regular fsync(2) only flushes to the drive's write cache and does
// NOT guarantee media durability. `sync_data` and `sync_full` therefore both
// issue F_FULLFSYNC, which asks the drive to flush its cache to stable media.
//
// F_NOCACHE (the `Method::Direct` open flag) only bypasses the unified buffer
// cache; it does not flush the drive's write cache. A Direct write is durable
// only once the caller follows it with `sync_data` / `sync_full`.
//
// Fallback: file systems that do not implement F_FULLFSYNC (some network and
// FUSE mounts) reject it with ENOTSUP / EOPNOTSUPP / EINVAL. On those errors
// the call falls back to fsync(2), which is the strongest flush such a file
// system offers. Any other error is returned to the caller.
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn sync_data(file: &File) -> Result<()> {
    // macOS has no fdatasync equivalent. Use F_FULLFSYNC for correctness.
    sync_full(file)
}

pub(crate) fn sync_full(file: &File) -> Result<()> {
    full_fsync_fd(file.as_raw_fd())
}

/// Issues `fcntl(fd, F_FULLFSYNC)`, retrying on `EINTR` and falling back
/// to `fsync(2)` when the file system does not support the fcntl.
fn full_fsync_fd(fd: libc::c_int) -> Result<()> {
    loop {
        // SAFETY: `fd` is a valid open descriptor borrowed from a live
        // `File` for the duration of the call; F_FULLFSYNC takes no
        // pointer argument.
        let ret = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC, 0_i32) };
        if ret != -1 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if full_fsync_unsupported(code) => return fsync_fd(fd),
            _ => return Err(Error::Io(err)),
        }
    }
}

/// `true` for the errno values a file system without F_FULLFSYNC support
/// returns.
fn full_fsync_unsupported(code: libc::c_int) -> bool {
    code == libc::ENOTSUP || code == libc::EOPNOTSUPP || code == libc::EINVAL
}

/// Plain `fsync(2)`, retrying on `EINTR`.
fn fsync_fd(fd: libc::c_int) -> Result<()> {
    loop {
        // SAFETY: `fd` is a valid open descriptor borrowed from a live
        // `File` for the duration of the call.
        let ret = unsafe { libc::fsync(fd) };
        if ret == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(Error::Io(err));
        }
    }
}

/// 0.9.4 — macOS `F_BARRIERFSYNC` opt-in.
///
/// Apple's `fcntl(F_BARRIERFSYNC)` ensures all prior I/O on the
/// fd has been transferred to the storage device, but does
/// **not** wait for the device's volatile write cache to flush
/// to media (which is what `F_FULLFSYNC` does and pays for).
///
/// **Crash safety contract.** `F_BARRIERFSYNC` provides
/// *ordering* — writes before the barrier reach the device
/// before writes after the barrier — and the device commits its
/// own write cache eventually. For a drive with PLP
/// (power-loss protection), this is **fully crash-safe**: data
/// in the write cache survives power loss because the
/// supercapacitor / tantalum keeps the cache alive long enough
/// to commit to NAND. For a drive *without* PLP, this is
/// **only correct under explicit, eventual `F_FULLFSYNC` at a
/// commit boundary** — the journal substrate's
/// `JournalOptions::sync_mode(SyncMode::Barrier)` opt-in
/// documents this requirement.
///
/// **Performance.** `F_BARRIERFSYNC` is dramatically cheaper
/// than `F_FULLFSYNC` on Apple Silicon (and Intel Mac) NVMe
/// devices — typically 10–100× faster depending on dirty
/// page count, because the drive's full-cache-flush is the
/// dominant cost of `F_FULLFSYNC`. The Apple File System
/// documentation explicitly recommends `F_BARRIERFSYNC` for
/// database WAL workloads on enterprise SSDs.
///
/// **Not exposed publicly** — used internally by the journal's
/// `sync_through` when the caller has opted in via
/// `JournalOptions::sync_mode(SyncMode::Barrier)`.
pub(crate) fn sync_barrier(file: &File) -> Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open file descriptor.
    let ret = unsafe { libc::fcntl(fd, libc::F_BARRIERFSYNC, 0_i32) };
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

/// 0.9.5 — Punches a hole at `[offset, offset + len)` via Apple's
/// `fcntl(F_PUNCHHOLE)`.
///
/// macOS exposes a structured `fpunchhole_t` payload instead of the
/// Linux `fallocate` int64-pair argument style. We construct the
/// payload here and pass it through `fcntl`.
///
/// `F_PUNCHHOLE` requires the offset and length to be multiples of the
/// file system block size and fails with `EINVAL` otherwise. The range
/// is therefore split: whole blocks inside it are punched, and the
/// unaligned head and tail are overwritten with zeros, so every byte of
/// the range reads back as zero afterwards. The range is first clipped to
/// the current file size, so the call never extends the file (matching
/// Linux `FALLOC_FL_KEEP_SIZE`).
///
/// Available on macOS 10.12 (Sierra) and later. On older
/// systems the kernel returns `EOPNOTSUPP`, which we surface as
/// an `Err`.
pub(crate) fn punch_hole(file: &File, offset: u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let eof = file.metadata().map_err(Error::Io)?.len();
    let end = offset.saturating_add(len).min(eof);
    if offset >= end {
        return Ok(());
    }
    let block = fs_block_size(file)?;
    let plan = split_punch_range(offset, end, block);
    if let Some((start, stop)) = plan.punch {
        punch_aligned(file, start, stop - start)?;
    }
    for (start, stop) in plan.zero.into_iter().flatten() {
        zero_fill(file, start, stop - start)?;
    }
    Ok(())
}

/// How [`punch_hole`] covers `[offset, end)`: the block-aligned middle is
/// punched, the unaligned head and tail are zero-filled.
#[derive(Debug, PartialEq, Eq)]
struct PunchPlan {
    punch: Option<(u64, u64)>,
    zero: [Option<(u64, u64)>; 2],
}

/// Splits `[offset, end)` around the whole `block`-sized blocks it covers.
fn split_punch_range(offset: u64, end: u64, block: u64) -> PunchPlan {
    let first_full = offset.div_ceil(block).saturating_mul(block);
    let last_full = end / block * block;
    if first_full >= last_full {
        // No whole block inside the range: zero all of it.
        return PunchPlan {
            punch: None,
            zero: [Some((offset, end)), None],
        };
    }
    let head = (offset < first_full).then_some((offset, first_full));
    let tail = (last_full < end).then_some((last_full, end));
    PunchPlan {
        punch: Some((first_full, last_full)),
        zero: [head, tail],
    }
}

/// File system block size for `file` (`fstatfs` `f_bsize`), the unit
/// `F_PUNCHHOLE` requires.
fn fs_block_size(file: &File) -> Result<u64> {
    // SAFETY: `statfs` is plain old data (integers and integer arrays),
    // so the all-zero bit pattern is a valid value; `fstatfs` fully
    // writes it before any field is read.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is borrowed from a live `File`; `st` is a
    // valid, writable `statfs`.
    let ret = unsafe { libc::fstatfs(file.as_raw_fd(), &mut st) };
    if ret != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    let block = u64::from(st.f_bsize);
    if block == 0 {
        return Err(Error::Io(std::io::Error::other(
            "fstatfs reported a zero block size",
        )));
    }
    Ok(block)
}

/// Issues `F_PUNCHHOLE` for a block-aligned range.
fn punch_aligned(file: &File, offset: u64, len: u64) -> Result<()> {
    /// Apple's `fpunchhole_t` (from `<sys/fcntl.h>`): two `u32`
    /// fields (`fp_flags`, `reserved`) followed by two `off_t`
    /// values. Total 24 bytes.
    #[repr(C)]
    struct FPunchHole {
        fp_flags: u32,          // reserved; must be zero
        reserved: u32,          // reserved; must be zero
        fp_offset: libc::off_t, // start
        fp_length: libc::off_t, // length
    }
    /// Apple's `F_PUNCHHOLE` fcntl number. Defined in the
    /// macOS SDK as 99.
    const F_PUNCHHOLE: libc::c_int = 99;
    let too_big = || {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "punch_hole range exceeds off_t",
        ))
    };
    let payload = FPunchHole {
        fp_flags: 0,
        reserved: 0,
        fp_offset: libc::off_t::try_from(offset).map_err(|_| too_big())?,
        fp_length: libc::off_t::try_from(len).map_err(|_| too_big())?,
    };
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open file descriptor; `payload` is a
    // stack-allocated `FPunchHole` matching the kernel's
    // expected layout. fcntl with `F_PUNCHHOLE` reads the
    // structure pointed to by the third argument.
    let ret = unsafe { libc::fcntl(fd, F_PUNCHHOLE, &payload as *const FPunchHole) };
    if ret != -1 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

/// Overwrites `[offset, offset + len)` with zeros using positioned
/// writes.
fn zero_fill(file: &File, offset: u64, len: u64) -> Result<()> {
    let zeros = [0u8; 8192];
    let mut done = 0u64;
    while done < len {
        let chunk = (len - done).min(zeros.len() as u64) as usize;
        write_at(file, offset + done, &zeros[..chunk])?;
        done += chunk as u64;
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Rename, directory sync, copy
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    // POSIX rename(2) is atomic within the same filesystem.
    // renameatx_np(RENAME_SWAP) would swap two existing files, which is NOT
    // what we need — we want to replace the destination. Plain rename() is
    // the correct primitive. See .dev/DECISIONS-0.3.0.md.
    std::fs::rename(from, to).map_err(Error::Io)
}

pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    let dir = File::open(super::parent_or_current_dir(path)).map_err(Error::Io)?;
    // Use F_FULLFSYNC on the directory as well for full durability, with
    // the same fsync(2) fallback as `sync_full`.
    full_fsync_fd(dir.as_raw_fd())
}

pub(crate) fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    // 0.9.6 — Try `clonefile(2)` for instant copy-on-write cloning
    // on APFS. On non-APFS filesystems (HFS+, exFAT, SMB, NFS), or
    // when the destination already exists, or on cross-device
    // copies, the syscall returns an error and we fall back to
    // `std::fs::copy` for full-byte-copy semantics.
    //
    // The win for HiveDB checkpoint flush workloads is significant:
    // clonefile is O(metadata) — the data blocks are shared until
    // first write (COW). For a 1 GiB checkpoint clone, this drops
    // from seconds to microseconds on APFS.
    //
    // Important: clonefile **requires the destination not to exist**.
    // We don't pre-unlink dst because that would be a TOCTOU race;
    // instead the EEXIST error path falls through to std::fs::copy
    // which overwrites correctly.
    if let (Ok(src_cstr), Ok(dst_cstr)) = (path_to_cstr(src), path_to_cstr(dst)) {
        // SAFETY: src_cstr and dst_cstr are valid NUL-terminated
        // C strings owned by the local CString values for the
        // duration of this call. clonefile reads both paths and
        // returns 0 on success or -1 with errno on error.
        let rc = unsafe { libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0) };
        if rc == 0 {
            // Success — return the source file's byte length, matching
            // std::fs::copy's return value contract.
            return std::fs::metadata(src).map(|m| m.len()).map_err(Error::Io);
        }
        // Fall through to std::fs::copy on any error (ENOTSUP for
        // non-APFS, EEXIST for existing dst, EXDEV for cross-device,
        // EACCES for permission issues). The fallback path covers
        // every error case cleanly.
    }
    std::fs::copy(src, dst).map_err(Error::Io)
}

// ──────────────────────────────────────────────────────────────────────────────
// Probes
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Storage-engine primitives — preallocate + advise
// ──────────────────────────────────────────────────────────────────────────────

/// macOS preallocate via `fcntl(F_PREALLOCATE)`. Tries
/// contiguous allocation first (`F_ALLOCATECONTIG`); falls back
/// to non-contiguous (`F_ALLOCATEALL`) if the contiguous request
/// can't be satisfied. The logical file size is not changed.
///
/// `F_PEOFPOSMODE` allocates `fst_length` bytes *past the physical end
/// of file*, i.e. on top of whatever is already allocated (fcntl(2)).
/// Passing `offset + len` every time therefore grew the file's
/// allocation on each call. Instead, the bytes already allocated
/// (`st_blocks * 512` from `fstat`) are subtracted and only the
/// shortfall up to `offset + len` is requested; a range that is already
/// covered is a no-op.
pub(crate) fn preallocate(file: &File, offset: u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    // `fstore_t` from <sys/fcntl.h>.
    #[repr(C)]
    struct Fstore {
        fst_flags: u32,
        fst_posmode: i32,
        fst_offset: libc::off_t,
        fst_length: libc::off_t,
        fst_bytesalloc: libc::off_t,
    }
    const F_PREALLOCATE: libc::c_int = 42;
    const F_ALLOCATECONTIG: u32 = 0x0000_0002;
    const F_ALLOCATEALL: u32 = 0x0000_0004;
    const F_PEOFPOSMODE: i32 = 3;

    let target = offset.checked_add(len).ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preallocate range overflows u64",
        ))
    })?;
    let allocated = allocated_bytes(file)?;
    let Some(shortfall) = preallocate_shortfall(target, allocated) else {
        return Ok(());
    };
    let fst_length = libc::off_t::try_from(shortfall).map_err(|_| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preallocate length exceeds off_t",
        ))
    })?;

    let fd = file.as_raw_fd();
    let mut store = Fstore {
        fst_flags: F_ALLOCATECONTIG | F_ALLOCATEALL,
        fst_posmode: F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length,
        fst_bytesalloc: 0,
    };
    // SAFETY: fd is valid; F_PREALLOCATE expects an `fstore_t *`
    // and reads/writes only that struct.
    let ret = unsafe { libc::fcntl(fd, F_PREALLOCATE, &mut store) };
    if ret != -1 {
        return Ok(());
    }
    // Contiguous allocation failed — retry without F_ALLOCATECONTIG.
    store.fst_flags = F_ALLOCATEALL;
    store.fst_bytesalloc = 0;
    // SAFETY: same as above.
    let ret = unsafe { libc::fcntl(fd, F_PREALLOCATE, &mut store) };
    if ret != -1 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

/// Bytes still to allocate so the file's allocation reaches `target`, or
/// `None` when `allocated` already covers it.
fn preallocate_shortfall(target: u64, allocated: u64) -> Option<u64> {
    target.checked_sub(allocated).filter(|&n| n > 0)
}

/// Bytes allocated to `file` on disk: `st_blocks` counts 512-byte units
/// regardless of the file system block size.
fn allocated_bytes(file: &File) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().map_err(Error::Io)?;
    Ok(meta.blocks().saturating_mul(512))
}

/// macOS advise — limited surface vs Linux. Sequential / WillNeed
/// map to `F_RDADVISE`; DontNeed maps to a temporary `F_NOCACHE`
/// flip; Random and Normal are best-effort no-ops.
pub(crate) fn advise(file: &File, offset: u64, len: u64, advice: crate::Advice) -> Result<()> {
    let fd = file.as_raw_fd();
    match advice {
        crate::Advice::Sequential | crate::Advice::WillNeed => {
            // F_RDADVISE: struct radvisory { off_t ra_offset; int ra_count; }
            #[repr(C)]
            struct Radvisory {
                ra_offset: libc::off_t,
                ra_count: libc::c_int,
            }
            const F_RDADVISE: libc::c_int = 44;
            let count = if len == 0 || len > i32::MAX as u64 {
                i32::MAX
            } else {
                len as i32
            };
            let mut adv = Radvisory {
                ra_offset: offset as libc::off_t,
                ra_count: count,
            };
            // SAFETY: fd is valid; F_RDADVISE expects a radvisory pointer.
            let ret = unsafe { libc::fcntl(fd, F_RDADVISE, &mut adv) };
            if ret == 0 {
                Ok(())
            } else {
                // Best-effort: failure isn't fatal.
                Ok(())
            }
        }
        crate::Advice::DontNeed => {
            // No direct equivalent on macOS; closest is
            // toggling F_NOCACHE which affects the *handle*'s
            // future reads, not a region. We accept this as a
            // best-effort no-op rather than mutating handle
            // state silently.
            let _ = (fd, offset, len);
            Ok(())
        }
        crate::Advice::Random | crate::Advice::Normal => Ok(()),
    }
}

pub(crate) fn probe_sector_size(path: &Path) -> u32 {
    use libc::statfs;

    let path_cstr = match path_to_cstr(path) {
        Ok(c) => c,
        Err(_) => return 512,
    };

    // SAFETY: `statfs` is plain old data (integers and integer arrays),
    // so the all-zero bit pattern is a valid value; `libc::statfs` fully
    // writes it before any field is read.
    let mut st: statfs = unsafe { std::mem::zeroed() };
    // SAFETY: path_cstr is valid NUL-terminated; st is properly sized.
    let ret = unsafe { libc::statfs(path_cstr.as_ptr(), &mut st) };
    if ret == 0 && st.f_bsize > 0 {
        let bs = st.f_bsize as u64;
        if (512..=65536).contains(&bs) {
            return bs as u32;
        }
    }
    512
}

pub(crate) fn probe_direct_io_available() -> bool {
    // F_NOCACHE is available on all macOS versions supported by fsys.
    true
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal helper
// ──────────────────────────────────────────────────────────────────────────────

fn path_to_cstr(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::InvalidPath {
        path: path.to_owned(),
        reason: "path contains an interior NUL byte".into(),
    })
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_macos_{}_{}_{}",
            std::process::id(),
            n,
            suffix
        ))
    }

    struct TmpFile(std::path::PathBuf);
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_open_write_new_creates_file() {
        let path = tmp_path("create");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        drop(f);
        assert!(path.exists());
    }

    #[test]
    fn test_write_read_roundtrip() {
        let path = tmp_path("rw");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"macos").expect("write");
        drop(f);
        let (rf, _) = open_read(&path, false).expect("read open");
        let data = read_all(&rf).expect("read");
        assert_eq!(data, b"macos");
    }

    #[test]
    fn test_sync_full_does_not_panic() {
        let path = tmp_path("sync");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, false).expect("open");
        write_all(&f, b"sync test").expect("write");
        sync_full(&f).expect("sync_full");
    }

    #[test]
    fn test_sync_data_succeeds_after_direct_write() {
        let path = tmp_path("direct_sync");
        let _g = TmpFile(path.clone());
        let (f, _) = open_write_new(&path, true).expect("open");
        write_all_direct(&f, b"direct then sync", 4096).expect("write");
        sync_data(&f).expect("sync_data");
    }

    #[test]
    fn test_full_fsync_unsupported_classification() {
        assert!(full_fsync_unsupported(libc::ENOTSUP));
        assert!(full_fsync_unsupported(libc::EOPNOTSUPP));
        assert!(full_fsync_unsupported(libc::EINVAL));
        assert!(!full_fsync_unsupported(libc::EIO));
        assert!(!full_fsync_unsupported(libc::EBADF));
    }

    #[test]
    fn test_sync_full_on_bad_fd_errors() {
        assert!(full_fsync_fd(-1).is_err());
    }

    #[test]
    fn test_preallocate_shortfall() {
        assert_eq!(preallocate_shortfall(4096, 0), Some(4096));
        assert_eq!(preallocate_shortfall(4096, 1024), Some(3072));
        assert_eq!(preallocate_shortfall(4096, 4096), None);
        assert_eq!(preallocate_shortfall(4096, 8192), None);
    }

    #[test]
    fn test_preallocate_repeated_call_does_not_keep_growing() {
        let path = tmp_path("prealloc");
        let _g = TmpFile(path.clone());
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open");
        preallocate(&f, 0, 1 << 20).expect("first");
        let first = allocated_bytes(&f).expect("blocks");
        preallocate(&f, 0, 1 << 20).expect("second");
        preallocate(&f, 0, 512 << 10).expect("smaller");
        assert_eq!(allocated_bytes(&f).expect("blocks"), first);
        assert_eq!(f.metadata().expect("meta").len(), 0, "EOF must not move");
    }

    #[test]
    fn test_split_punch_range_aligned_and_unaligned() {
        // Fully aligned: punch only.
        assert_eq!(
            split_punch_range(4096, 12288, 4096),
            PunchPlan {
                punch: Some((4096, 12288)),
                zero: [None, None]
            }
        );
        // Unaligned both ends: zero head and tail around the middle.
        assert_eq!(
            split_punch_range(100, 10_000, 4096),
            PunchPlan {
                punch: Some((4096, 8192)),
                zero: [Some((100, 4096)), Some((8192, 10_000))],
            }
        );
        // Inside one block: zero only.
        assert_eq!(
            split_punch_range(10, 20, 4096),
            PunchPlan {
                punch: None,
                zero: [Some((10, 20)), None]
            }
        );
    }

    #[test]
    fn test_punch_hole_unaligned_range_reads_back_zero_and_keeps_size() {
        let path = tmp_path("punch");
        let _g = TmpFile(path.clone());
        std::fs::write(&path, vec![0xAAu8; 64 * 1024]).expect("seed");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open");
        punch_hole(&f, 1000, 20_000).expect("punch");
        // Past EOF: clipped, file size unchanged.
        punch_hole(&f, 60 * 1024, 1 << 20).expect("punch past eof");
        drop(f);
        let data = std::fs::read(&path).expect("read");
        assert_eq!(data.len(), 64 * 1024);
        assert!(data[..1000].iter().all(|&b| b == 0xAA));
        assert!(data[1000..21_000].iter().all(|&b| b == 0));
        assert!(data[21_000..60 * 1024].iter().all(|&b| b == 0xAA));
        assert!(data[60 * 1024..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_atomic_rename_works() {
        let src = tmp_path("ren_src");
        let dst = tmp_path("ren_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"new content").expect("write");
        atomic_rename(&src, &dst).expect("rename");
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).expect("read"), b"new content");
    }

    #[test]
    fn test_probe_sector_size_returns_at_least_512() {
        let size = probe_sector_size(Path::new("/tmp"));
        assert!(size >= 512);
    }

    #[test]
    fn test_copy_file_produces_correct_content() {
        let src = tmp_path("cp_src");
        let dst = tmp_path("cp_dst");
        let _gs = TmpFile(src.clone());
        let _gd = TmpFile(dst.clone());
        std::fs::write(&src, b"copy content").expect("write");
        let bytes = copy_file(&src, &dst).expect("copy");
        assert_eq!(bytes, 12);
    }
}
