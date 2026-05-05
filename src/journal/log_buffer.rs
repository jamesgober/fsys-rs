//! Sector-aligned log buffer used by Direct-IO journal mode.
//!
//! Architecturally analogous to InnoDB's redo-log buffer: a fixed
//! in-memory region protected by a single mutex into which records
//! are serialised, then flushed to disk in sector-aligned chunks.
//! Replaces the lock-free `pwrite`-per-append fast path used by
//! buffered-mode journals — the tradeoff is mutex-serialised
//! appends in exchange for zero-copy DMA to the device.
//!
//! ## Invariants
//!
//! - `buf` is allocated via [`AlignedBuf`], guaranteeing pointer +
//!   length alignment to the device's sector size. Required for
//!   `O_DIRECT` / `FILE_FLAG_NO_BUFFERING` writes.
//! - `flush_pos` is always sector-aligned. Writes always start at a
//!   sector boundary.
//! - `len` is the number of valid (record-bearing) bytes in `buf`,
//!   measured from `buf[0]`. `len ≤ buf.len()`.
//! - The on-disk byte range `file[0..flush_pos + len_at_last_flush]`
//!   contains the journal's records (with zero-padding inside the
//!   last partial sector when the last flush was a sync-triggered
//!   partial flush). Subsequent buffer-full real flushes overwrite
//!   the partial-sector pad with the next records.
//!
//! ## Why a partial flush keeps `flush_pos` unchanged
//!
//! A `sync_through` issued while `len < buf.len()` writes
//! `aligned_len(len)` bytes (the records plus zero-pad to the next
//! sector). The next append continues filling `buf` from `len` —
//! NOT from a fresh sector boundary — because LSNs are byte-precise
//! and an LSN gap would corrupt resume-after-crash semantics.
//!
//! When the buffer eventually fills (`len == buf.len()`), the real
//! flush writes the entire `buf` at `flush_pos`, overwriting the
//! prior partial-sector zero-pad with the now-real data, and
//! advances `flush_pos += buf.len()` for the next round.

#![allow(dead_code)] // accessor helpers are reserved for benches / future direct-mode probes

use crate::journal::format;
use crate::platform::{round_up, AlignedBuf};
use crate::{Error, Result};
use std::fs::File;

/// Sector-aligned in-memory buffer for the Direct-IO journal write
/// path.
pub(crate) struct LogBuffer {
    /// Sector-aligned scratch — pointer, length, and any flushed
    /// region are all sector-aligned. Total capacity = `buf.len`.
    buf: AlignedBuf,
    /// Bytes from `buf[0..len]` are records-bearing; bytes from
    /// `buf[len..]` are scratch (typically zeroed from
    /// `alloc_zeroed`, then re-written as records arrive).
    len: usize,
    /// File offset of `buf[0]`. Always sector-aligned. Advances by
    /// `buf.len()` on a real (full-buffer) flush; unchanged by a
    /// partial flush.
    flush_pos: u64,
    /// Sector size of the underlying device. Hot-path constant —
    /// captured at construction.
    sector_size: usize,
}

impl LogBuffer {
    /// Allocates a fresh log buffer of `capacity` bytes (must be a
    /// sector-multiple) starting at file offset `flush_pos` (must
    /// also be sector-aligned).
    ///
    /// Capacity is rounded UP to the next sector boundary if it
    /// isn't already aligned (defensive — callers compute it in
    /// `JournalHandle::open_with_options` from
    /// `JournalOptions::log_buffer_kib`).
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] if the aligned allocation fails.
    pub(crate) fn new(capacity: u32, sector_size: u32, flush_pos: u64) -> Result<Self> {
        let ss = sector_size as usize;
        debug_assert!(ss.is_power_of_two(), "sector_size must be a power of two");
        debug_assert!(
            flush_pos % sector_size as u64 == 0,
            "flush_pos must be sector-aligned"
        );
        let cap = round_up(capacity as usize, ss).max(ss);
        let buf = AlignedBuf::new(cap, ss)?;
        Ok(Self {
            buf,
            len: 0,
            flush_pos,
            sector_size: ss,
        })
    }

    /// Returns the LSN that the next append would place its
    /// record's first byte at.
    #[inline]
    pub(crate) fn next_lsn(&self) -> u64 {
        self.flush_pos + self.len as u64
    }

    /// Returns the buffer's total capacity in bytes.
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.buf.len
    }

    /// Returns the file offset of the first byte that has been
    /// flushed to disk. Bytes at `[0..flushed_through())` are on
    /// stable storage *after* a `sync_data` syscall completes.
    #[inline]
    pub(crate) fn flushed_through(&self) -> u64 {
        self.flush_pos
    }

    /// Returns the number of buffered (not-yet-flushed) bytes.
    #[inline]
    pub(crate) fn buffered_len(&self) -> usize {
        self.len
    }

    /// Encodes `payload` as a frame and appends to the buffer,
    /// flushing first if the frame doesn't fit. If the frame is
    /// larger than the entire buffer, falls back to a direct
    /// sector-aligned standalone write (records of this size
    /// are uncommon in WAL workloads).
    ///
    /// Returns `(start_lsn, end_lsn)` — the file-byte-offset range
    /// the frame occupies.
    pub(crate) fn append_frame(&mut self, file: &File, payload: &[u8]) -> Result<(u64, u64)> {
        let frame_size = payload.len() + format::FRAME_OVERHEAD;

        // Encode the frame on the caller's stack/heap (CRC compute
        // outside the lock would be ideal but the API takes
        // payload, not pre-encoded frame; this keeps the call sites
        // simple).
        let frame = format::encode_frame_owned(payload)?;
        debug_assert_eq!(frame.len(), frame_size);

        // Path A: the frame fits in the buffer's remaining space.
        if self.len + frame_size <= self.buf.len {
            let start = self.flush_pos + self.len as u64;
            let end = start + frame_size as u64;
            self.buf.as_mut_slice()[self.len..self.len + frame_size].copy_from_slice(&frame);
            self.len += frame_size;
            return Ok((start, end));
        }

        // Path B: the frame doesn't fit — flush whatever's
        // currently buffered, then either fit in the now-empty
        // buffer (B1) or do a standalone sector-aligned write (B2).
        if self.len > 0 {
            self.flush_full(file)?;
        }
        debug_assert_eq!(self.len, 0);

        if frame_size <= self.buf.len {
            // B1: empty buffer is large enough; copy in.
            let start = self.flush_pos;
            let end = start + frame_size as u64;
            self.buf.as_mut_slice()[..frame_size].copy_from_slice(&frame);
            self.len = frame_size;
            Ok((start, end))
        } else {
            // B2: oversize record. Allocate a one-shot
            // sector-aligned scratch, copy + zero-pad, write
            // directly. Advance `flush_pos` by the aligned write
            // length; the LSN reported back is `start + frame_size`
            // (the unpadded record end), which is what subsequent
            // appends should resume from.
            //
            // Caveat: oversize records leave a partial trailing
            // sector containing zeros after the record. The next
            // append's first sector flush will overwrite that
            // partial sector with new record data — same invariant
            // as the partial-sync flush.
            let aligned = round_up(frame_size, self.sector_size);
            let mut scratch = AlignedBuf::new(aligned, self.sector_size)?;
            scratch.as_mut_slice()[..frame_size].copy_from_slice(&frame);
            // `alloc_zeroed` already filled the trailing pad.
            let start = self.flush_pos;
            crate::platform::write_at_direct(file, self.flush_pos, scratch.as_slice())?;
            // Advance `flush_pos` by the *unpadded* frame size: the
            // next record continues from where the frame ended;
            // partial-sector pad will be overwritten on the next
            // flush. But that means `flush_pos` is no longer sector-
            // aligned. Restore the invariant by truncating to the
            // last sector boundary AND re-loading the partial sector
            // into the buffer (so subsequent appends naturally
            // overwrite the pad).
            let end = start + frame_size as u64;
            let new_flush_pos = (end / self.sector_size as u64) * self.sector_size as u64;
            self.flush_pos = new_flush_pos;
            // The buffer holds the partial trailing sector's bytes:
            // [end - new_flush_pos] bytes of frame tail. Reload them
            // from `scratch`.
            let tail = (end - new_flush_pos) as usize;
            if tail > 0 {
                let scratch_offset = aligned - self.sector_size;
                let in_sector = (frame_size - scratch_offset).min(self.sector_size);
                self.buf.as_mut_slice()[..in_sector].copy_from_slice(
                    &scratch.as_slice()[scratch_offset..scratch_offset + in_sector],
                );
                // Zero the rest of the buffer's first sector.
                for b in &mut self.buf.as_mut_slice()[in_sector..self.sector_size] {
                    *b = 0;
                }
                self.len = tail;
            }
            Ok((start, end))
        }
    }

    /// Real flush: the buffer is full (or being forced empty for
    /// an oversize-record path). Writes the whole `buf`, advances
    /// `flush_pos`, resets `len = 0`.
    fn flush_full(&mut self, file: &File) -> Result<()> {
        let cap = self.buf.len;
        // The buffer's tail (if any) is whatever was left over
        // from a prior partial flush — it's already in valid
        // record bytes from the append path.
        crate::platform::write_at_direct(file, self.flush_pos, &self.buf.as_slice()[..cap])?;
        self.flush_pos = self
            .flush_pos
            .checked_add(cap as u64)
            .ok_or_else(|| Error::Io(std::io::Error::other("flush_pos overflow")))?;
        self.len = 0;
        // Zero the buffer for next round so any unused tail bytes
        // (after a future partial flush) are pad-zero rather than
        // stale data.
        for b in self.buf.as_mut_slice().iter_mut() {
            *b = 0;
        }
        Ok(())
    }

    /// Repositions the buffer for resume-after-crash. Called by
    /// `JournalHandle::open_direct` after `scan_clean_end` finds
    /// the last good LSN. Sets `flush_pos` to the last sector
    /// boundary at or before `resume_lsn`, primes the buffer's
    /// first sector with the partial-sector tail (`prefix_bytes`)
    /// so subsequent flushes overwrite the existing on-disk
    /// zero-pad cleanly, and sets `len` to the in-sector
    /// resume offset.
    pub(crate) fn set_flush_pos_for_resume(
        &mut self,
        flush_pos: u64,
        in_sector_offset: usize,
        prefix_bytes: &[u8],
    ) {
        debug_assert_eq!(self.len, 0, "rehydrate must run on a fresh buffer");
        debug_assert!(
            flush_pos % self.sector_size as u64 == 0,
            "flush_pos must be sector-aligned"
        );
        self.flush_pos = flush_pos;
        if in_sector_offset > 0 {
            let copy_len = in_sector_offset
                .min(prefix_bytes.len())
                .min(self.sector_size);
            self.buf.as_mut_slice()[..copy_len].copy_from_slice(&prefix_bytes[..copy_len]);
            self.len = copy_len;
        }
    }

    /// Partial / sync-point flush: writes `aligned_len(len)` bytes
    /// at `flush_pos` (records + zero-pad to sector boundary).
    /// Does NOT advance `flush_pos` or reset `len` — the next
    /// append continues filling the buffer from `len`. Subsequent
    /// flushes overwrite the partial-sector pad with new record
    /// bytes.
    pub(crate) fn flush_partial(&mut self, file: &File) -> Result<()> {
        if self.len == 0 {
            return Ok(()); // nothing to flush
        }
        let aligned = round_up(self.len, self.sector_size);
        // Bytes [self.len..aligned] are pad. The buffer was
        // zero-initialised on construction or after `flush_full`;
        // bytes between successive partial flushes can become
        // non-zero only via `append_frame` → `copy_from_slice`,
        // which writes exactly `frame_size` bytes. So the
        // [self.len..aligned] tail is zero by induction.
        crate::platform::write_at_direct(file, self.flush_pos, &self.buf.as_slice()[..aligned])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static C: AtomicU32 = AtomicU32::new(0);

    fn tmp_path(tag: &str) -> PathBuf {
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("fsys_logbuf_{}_{}_{tag}", std::process::id(), n))
    }

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn make_file() -> (PathBuf, File, Cleanup) {
        let path = tmp_path("logbuf");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        (path.clone(), f, Cleanup(path))
    }

    #[test]
    fn new_buffer_is_aligned_and_zeroed() {
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        assert_eq!(buf.capacity(), 4096);
        assert_eq!(buf.next_lsn(), 0);
        assert_eq!(buf.buffered_len(), 0);
    }

    #[test]
    fn append_frame_fits_in_buffer() {
        let (path, file, _g) = make_file();
        let mut buf = LogBuffer::new(4096, 512, 0).unwrap();
        let (start, end) = buf.append_frame(&file, b"hello").unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, 5 + format::FRAME_OVERHEAD as u64);
        assert_eq!(buf.next_lsn(), end);
        // No flush happened yet — file is empty.
        let on_disk = std::fs::read(&path).unwrap();
        assert!(on_disk.is_empty());
    }

    #[test]
    fn buffer_full_triggers_real_flush_and_advances_flush_pos() {
        let (path, file, _g) = make_file();
        let mut buf = LogBuffer::new(4096, 512, 0).unwrap();
        // Append records until we cross the buffer boundary.
        // Each frame: 12 byte overhead + 100 byte payload = 112 bytes.
        // 4096 / 112 = 36 records fit; the 37th triggers a flush.
        let payload = vec![0xABu8; 100];
        for _ in 0..36 {
            let _ = buf.append_frame(&file, &payload).unwrap();
        }
        assert_eq!(buf.flushed_through(), 0); // no flush yet
                                              // 37th append triggers flush.
        let _ = buf.append_frame(&file, &payload).unwrap();
        assert_eq!(buf.flushed_through(), 4096);
        // File size now = 4096 (real flush wrote the full buffer).
        let mut f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 4096);
    }

    #[test]
    fn flush_partial_writes_records_plus_zero_pad() {
        let (path, file, _g) = make_file();
        let mut buf = LogBuffer::new(4096, 512, 0).unwrap();
        let _ = buf.append_frame(&file, b"x").unwrap(); // 13-byte frame
        buf.flush_partial(&file).unwrap();
        // Aligned-up to next sector = 512 bytes written.
        let mut f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 512);
        // First 13 bytes = the frame; remaining 499 bytes = zero pad.
        assert!(bytes[13..].iter().all(|&b| b == 0));
        // flush_pos NOT advanced (partial flush invariant).
        assert_eq!(buf.flushed_through(), 0);
    }

    #[test]
    fn oversize_record_writes_directly_with_partial_sector_carryover() {
        let (path, file, _g) = make_file();
        let mut buf = LogBuffer::new(4096, 512, 0).unwrap();
        // Append a 5000-byte payload — frame is 5012 bytes,
        // which exceeds the 4096-byte buffer.
        let payload = vec![0xCDu8; 5000];
        let (start, end) = buf.append_frame(&file, &payload).unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, 5012);
        // flush_pos lands at the last sector boundary that ≤ end:
        // floor(5012 / 512) * 512 = 4608.
        assert_eq!(buf.flushed_through(), 4608);
        // The buffer's first sector now contains the partial-sector
        // tail (404 bytes of frame), at len = 5012 - 4608 = 404.
        assert_eq!(buf.buffered_len(), 404);
        // File on disk is sector-aligned-up: round_up(5012, 512) = 5120.
        let mut f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 5120);
    }
}
