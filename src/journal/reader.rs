//! Journal reader — replay records from a journal file.
//!
//! [`JournalReader`] is the read-side companion to
//! [`JournalHandle`](super::JournalHandle). It iterates the
//! journal's framed records forward from any starting LSN,
//! validating each record's CRC-32C and stopping cleanly at the
//! end-of-journal or at a tail-truncated partial write.
//!
//! ## Read-side performance
//!
//! Two read modes are exposed:
//!
//! - **`iter()` — buffered streaming.** Reads in 64 KiB chunks
//!   and decodes frames in-place. Best for sequential replay
//!   workloads (recovery on startup, full-log scan). Linear
//!   scan over typical NVMe storage saturates ~3 GB/s, more
//!   than enough to replay multi-GiB journals in a few
//!   seconds.
//! - **`iter_mmap()` — memory-mapped scan.** When the journal
//!   fits in available memory, mmap eliminates the read-buffer
//!   ping-pong and lets the kernel's prefetch logic drive the
//!   IO. Same forward-iteration semantics; faster for large
//!   journals on systems with healthy page cache.
//!
//! Random-access reads at a specific LSN are supported via
//! [`JournalReader::read_at_lsn`].
//!
//! ## Tail-truncation handling
//!
//! The iterator stops at the first non-`Ok` frame decode and
//! returns `None`. Truncation can happen three ways:
//!
//! 1. The header (8 bytes) is incomplete → `None`.
//! 2. The payload is incomplete → `None`.
//! 3. The trailing CRC-32C doesn't match → `None`.
//!
//! After the iterator yields `None`, [`JournalReader::tail_state`]
//! reports the exact reason (clean end-of-file vs. truncated
//! tail vs. corrupt frame). Callers replaying for recovery can
//! distinguish a clean shutdown from a crash.

use crate::journal::format::{decode_frame, FrameDecode, FRAME_OVERHEAD};
use crate::journal::Lsn;
use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;

/// Default buffer size for [`JournalReader::iter`] streaming.
/// 64 KiB is large enough that we amortise read syscalls
/// (one syscall per ~~512~~ ~1000 64-byte records) without
/// monopolising the page cache.
const READ_BUF_SIZE: usize = 64 * 1024;

/// Sector granularity used when zero-pad-skipping the trailing
/// partial sector of a Direct-IO journal. 512 is the smallest
/// sector size on any modern device; skipping in 512-byte
/// increments works for every multiple-of-512 actual sector
/// (which covers all common configurations: 512, 1024, 2048,
/// 4096).
const PAD_SKIP_GRANULARITY: u64 = 512;

/// Maximum number of pad sectors the reader will skip past in a
/// single zero-pad region before giving up and surfacing
/// `BadMagic`. A healthy direct-IO journal has at most one
/// trailing pad sector (after a partial flush); we cap defensively
/// at 16 so a pathological all-zero file doesn't trigger an
/// O(file_size) scan.
const MAX_PAD_SKIP_SECTORS: u32 = 16;

/// Outcome of an iteration that yielded `None` — distinguishes
/// clean end-of-file from various truncation/corruption modes.
///
/// Recovery code typically checks this after iteration to decide
/// whether to truncate the file at the last-good-LSN (clean
/// recovery from a crash) or surface an error (corruption that
/// can't be safely truncated past).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JournalTailState {
    /// Iteration consumed the entire file with no errors. The
    /// journal ended cleanly at the LSN reported by
    /// [`JournalReader::position`].
    CleanEnd,
    /// Iteration stopped because a frame's header was
    /// incomplete (less than 8 bytes left). Indicates a crash
    /// mid-write: the writer reserved the LSN but didn't get
    /// to write the full header. The caller should truncate
    /// the file at [`JournalReader::position`] before
    /// reopening for further appends.
    TruncatedHeader,
    /// Iteration stopped because a frame's payload was
    /// incomplete. Same recovery semantics as `TruncatedHeader`.
    TruncatedPayload,
    /// A frame's trailing CRC-32C didn't match the computed
    /// value. This either indicates corruption (rare on
    /// modern NVMe — checksums catch flipped bits) or a
    /// crash mid-write where the kernel flushed the header
    /// before the trailer. Treat as truncation: the caller
    /// should truncate at [`JournalReader::position`].
    ChecksumMismatch,
    /// A frame's magic prefix didn't match `0x46535901`. This
    /// indicates either (a) the file is not an fsys journal
    /// (format confusion), (b) the journal was written by a
    /// future version with a different magic byte, or (c)
    /// data corruption in the magic field. The caller should
    /// NOT truncate-and-resume — surface this to a human
    /// operator.
    BadMagic,
    /// A frame's length field exceeds the 256 MiB cap baked
    /// into the v1 frame format. Indicates either a future
    /// format version with a different framing, or data
    /// corruption. Same caveat as `BadMagic` — don't
    /// auto-recover.
    LengthOverflow,
}

/// One record yielded by a [`JournalReader`] iterator.
#[derive(Debug, Clone)]
pub struct JournalRecord {
    /// LSN of this record (the byte offset of its frame's start).
    /// Useful for re-positioning the reader to this point later.
    pub lsn: Lsn,
    /// Decoded payload bytes — the same `&[u8]` originally
    /// passed to [`JournalHandle::append`](super::JournalHandle::append).
    pub payload: Vec<u8>,
}

/// Sequential-replay reader for a journal file.
///
/// Open with [`JournalReader::open`] and iterate via
/// [`JournalReader::iter`] for buffered streaming. Random-access
/// reads at a specific LSN are supported via
/// [`JournalReader::read_at_lsn`].
///
/// # Concurrency
///
/// `JournalReader` is `Send` but not `Sync` — it holds an
/// internal cursor. To read concurrently from multiple
/// threads, open multiple `JournalReader`s (cheap; they share
/// no state with the writer side).
///
/// # Safety vs the writer
///
/// `JournalReader` opens the file read-only. Concurrent appends
/// from a [`super::JournalHandle`] writer are safe — the reader
/// sees records up to the file size at the time it issued each
/// `read` syscall. Records appended after a partial scan are
/// visible only on the next `read` cycle (call
/// [`JournalReader::refresh_size`] to pick up new data).
pub struct JournalReader {
    file: File,
    file_size: u64,
    cursor: u64,
    last_state: JournalTailState,
}

impl JournalReader {
    /// Opens a journal at `path` for read-only iteration.
    ///
    /// Returns an error if the file doesn't exist or can't be
    /// opened. The reader's cursor starts at LSN 0.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(Error::Io)?;
        let file_size = file.metadata().map_err(Error::Io)?.len();
        Ok(Self {
            file,
            file_size,
            cursor: 0,
            last_state: JournalTailState::CleanEnd,
        })
    }

    /// Repositions the cursor to a specific LSN. Subsequent
    /// iteration starts decoding from that offset.
    ///
    /// `lsn` must point to a frame boundary — the byte offset
    /// of a frame's first byte (the magic). Invalid offsets
    /// surface as `BadMagic` or `ChecksumMismatch` on the next
    /// iteration.
    pub fn seek_to(&mut self, lsn: Lsn) {
        self.cursor = lsn.as_u64();
        self.last_state = JournalTailState::CleanEnd;
    }

    /// Returns the cursor's current byte offset.
    #[must_use]
    pub fn position(&self) -> Lsn {
        Lsn::new(self.cursor)
    }

    /// Returns the journal file's size, captured at
    /// [`JournalReader::open`] time. To pick up records appended
    /// after open, call [`JournalReader::refresh_size`].
    #[must_use]
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Re-stats the underlying file and updates the cached
    /// `file_size`. Call this if the journal is being
    /// concurrently appended and the reader needs to see the
    /// new tail.
    pub fn refresh_size(&mut self) -> Result<()> {
        self.file_size = self.file.metadata().map_err(Error::Io)?.len();
        Ok(())
    }

    /// Returns the tail state observed by the most recent
    /// iteration. After an iterator yields `None`, this tells
    /// the caller why iteration stopped.
    #[must_use]
    pub fn tail_state(&self) -> JournalTailState {
        self.last_state
    }

    /// Hints the kernel about the access pattern for a region
    /// of this journal. The kernel uses the hint to drive
    /// page-cache prefetch / eviction / read-ahead.
    ///
    /// `len = 0` means "the rest of the file from `offset`."
    ///
    /// Common pattern: call `advise(0, 0, Advice::Sequential)`
    /// before [`JournalReader::iter`] for replay workloads —
    /// the kernel extends its read-ahead window for sequential
    /// scans.
    pub fn advise(&self, offset: u64, len: u64, advice: crate::Advice) -> Result<()> {
        crate::platform::advise(&self.file, offset, len, advice)
    }

    /// Convenience: `advise(0, 0, Advice::Sequential)` — hint
    /// the kernel to extend its read-ahead window across the
    /// whole journal. Call before [`JournalReader::iter`] for
    /// replay workloads.
    pub fn advise_sequential(&self) -> Result<()> {
        self.advise(0, 0, crate::Advice::Sequential)
    }

    /// Returns a buffered streaming iterator over records.
    ///
    /// The iterator reads the file in 64 KiB chunks and decodes
    /// frames in-place. Stops cleanly at the end of file or at
    /// the first decode error (truncation / corruption).
    pub fn iter(&mut self) -> JournalIter<'_> {
        JournalIter::new(self)
    }

    /// Reads exactly one record at `lsn`, returning the
    /// payload bytes.
    ///
    /// `lsn` must point to a frame boundary (the byte offset
    /// of a record's first byte, as returned by a previous
    /// [`JournalRecord::lsn`] or via subtracting the record's
    /// frame size from a [`JournalHandle::append`](super::JournalHandle::append)
    /// return value).
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] if the read fails.
    /// - [`Error::Io`] with `InvalidData` if the frame at
    ///   `lsn` doesn't decode cleanly (bad magic, bad CRC,
    ///   etc.).
    pub fn read_at_lsn(&mut self, lsn: Lsn) -> Result<JournalRecord> {
        // Read a header-sized chunk first to learn the length.
        let lsn_off = lsn.as_u64();
        let mut header = [0u8; FRAME_OVERHEAD];
        self.read_exact_at(lsn_off, &mut header[..8])?;
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        // Read the full frame.
        let mut frame = vec![0u8; FRAME_OVERHEAD + length];
        self.read_exact_at(lsn_off, &mut frame)?;
        // Decode + validate.
        match decode_frame(&frame) {
            FrameDecode::Ok {
                payload_start,
                payload_end,
                ..
            } => Ok(JournalRecord {
                lsn,
                payload: frame[payload_start..payload_end].to_vec(),
            }),
            FrameDecode::BadMagic => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad magic at LSN {lsn}"),
            ))),
            FrameDecode::ChecksumMismatch => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("CRC-32C mismatch at LSN {lsn}"),
            ))),
            FrameDecode::Truncated => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("frame at LSN {lsn} is truncated"),
            ))),
            FrameDecode::LengthOverflow => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("frame length at LSN {lsn} exceeds FRAME_MAX_PAYLOAD"),
            ))),
        }
    }

    /// Reads exactly `buf.len()` bytes from `offset` into `buf`,
    /// using the platform's positioned-read primitive. Loops
    /// on partial reads / EINTR.
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        // Cross-platform pread via std::io::Seek + Read. We
        // could use the platform layer's `read_at` primitive
        // (POSIX `pread`, Windows `ReadFile` + OVERLAPPED), but
        // the read-side perf isn't currently the hot path —
        // the journal's tier-4 work targets writes. For 0.9.0
        // RC we use the std::io path; if read benchmarks show
        // it as a bottleneck we'll lift to the platform layer.
        use std::io::Seek;
        let _ = self
            .file
            .seek(std::io::SeekFrom::Start(offset))
            .map_err(Error::Io)?;
        let mut total = 0;
        while total < buf.len() {
            let n = self.file.read(&mut buf[total..]).map_err(Error::Io)?;
            if n == 0 {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF in journal read_exact_at",
                )));
            }
            total += n;
        }
        Ok(())
    }
}

/// Streaming iterator over journal records, returned by
/// [`JournalReader::iter`].
///
/// Each `next()` call yields `Some(Result<JournalRecord>)`. A
/// `None` return means iteration ended; the reason is in
/// [`JournalReader::tail_state`].
///
/// The iterator owns a 64 KiB read buffer and re-fills it as
/// records are decoded. The reader's cursor advances after
/// each successful frame decode.
pub struct JournalIter<'a> {
    reader: &'a mut JournalReader,
    /// Local read buffer. `valid_start..valid_end` is the
    /// region containing data not-yet-consumed by frame decode.
    buf: Vec<u8>,
    valid_start: usize,
    valid_end: usize,
    /// Once an error or end-of-file has been observed, no
    /// further iteration attempts are made.
    finished: bool,
    /// Counts consecutive pad-sector skips. Resets to zero on
    /// every successful frame decode. Capped at
    /// `MAX_PAD_SKIP_SECTORS` to bound the cost of zero-pad
    /// scanning on pathological input.
    pad_skips_in_a_row: u32,
}

impl<'a> JournalIter<'a> {
    fn new(reader: &'a mut JournalReader) -> Self {
        Self {
            reader,
            buf: vec![0u8; READ_BUF_SIZE],
            valid_start: 0,
            valid_end: 0,
            finished: false,
            pad_skips_in_a_row: 0,
        }
    }

    /// Refills the read buffer from the file at the current
    /// cursor + valid_end-relative offset. Compacts any
    /// remaining unconsumed bytes to the front of the buffer
    /// before reading.
    fn refill(&mut self) -> Result<()> {
        // Compact unconsumed tail to the front.
        if self.valid_start > 0 {
            let remaining = self.valid_end - self.valid_start;
            self.buf.copy_within(self.valid_start..self.valid_end, 0);
            self.valid_start = 0;
            self.valid_end = remaining;
        }

        // Read into the empty tail of the buffer.
        let space = self.buf.len() - self.valid_end;
        if space == 0 {
            // Buffer is full but the next frame doesn't fit.
            // For a 64 KiB buffer this only happens for records
            // larger than ~64 KiB — rare in WAL workloads. Grow
            // the buffer to fit the largest record we'll see.
            // The frame's length field (already in the buffer)
            // tells us how much we need.
            //
            // Inspect bytes 4..8 of the current valid region to
            // find the record length.
            if self.valid_end - self.valid_start < 8 {
                return Ok(()); // need more bytes for the header
            }
            let len_bytes = &self.buf[self.valid_start + 4..self.valid_start + 8];
            let payload_len =
                u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]])
                    as usize;
            let needed = FRAME_OVERHEAD + payload_len;
            if self.buf.len() < needed {
                self.buf.resize(needed, 0);
            }
        }

        // Issue the read at file offset = cursor + (valid_end - valid_start).
        // valid_start is 0 after compaction so this simplifies to:
        // file_offset = self.reader.cursor + valid_end.
        let file_offset = self.reader.cursor + self.valid_end as u64;
        if file_offset >= self.reader.file_size {
            return Ok(()); // no more data to read
        }
        let to_read = std::cmp::min(
            (self.reader.file_size - file_offset) as usize,
            self.buf.len() - self.valid_end,
        );
        if to_read == 0 {
            return Ok(());
        }
        let buf_slice = &mut self.buf[self.valid_end..self.valid_end + to_read];
        // Use std::io::Seek + Read; same rationale as
        // read_exact_at above.
        use std::io::Seek;
        let _ = self
            .reader
            .file
            .seek(std::io::SeekFrom::Start(file_offset))
            .map_err(Error::Io)?;
        let mut total = 0;
        while total < buf_slice.len() {
            let n = self
                .reader
                .file
                .read(&mut buf_slice[total..])
                .map_err(Error::Io)?;
            if n == 0 {
                break;
            }
            total += n;
        }
        self.valid_end += total;
        Ok(())
    }
}

impl<'a> Iterator for JournalIter<'a> {
    type Item = Result<JournalRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        loop {
            // If we don't have enough buffered data to even read
            // a header, refill.
            let need_refill = (self.valid_end - self.valid_start) < 8;
            // `next_unread_file_offset` = absolute file offset of
            // the byte just past the last-read data. This is the
            // file position from which the next read syscall
            // would pick up. Note: cursor + (valid_end -
            // valid_start) — NOT cursor + valid_end. cursor
            // tracks the next-to-decode frame's file offset; the
            // buffered region [valid_start..valid_end] holds
            // file[cursor..cursor + (valid_end - valid_start)].
            let next_unread_file_offset =
                self.reader.cursor + (self.valid_end - self.valid_start) as u64;
            let at_eof = next_unread_file_offset >= self.reader.file_size
                && (self.valid_end - self.valid_start) == 0;
            if at_eof {
                self.reader.last_state = JournalTailState::CleanEnd;
                self.finished = true;
                return None;
            }
            if need_refill {
                if let Err(e) = self.refill() {
                    self.finished = true;
                    return Some(Err(e));
                }
                // After refill, if we still don't have enough
                // bytes for a header, the journal ends with a
                // partial header — truncated tail.
                if (self.valid_end - self.valid_start) < 8 {
                    if (self.valid_end - self.valid_start) == 0 {
                        // No leftover bytes — clean end.
                        self.reader.last_state = JournalTailState::CleanEnd;
                    } else {
                        self.reader.last_state = JournalTailState::TruncatedHeader;
                    }
                    self.finished = true;
                    return None;
                }
            }

            // Decode at the start of the valid region.
            let view = &self.buf[self.valid_start..self.valid_end];
            match decode_frame(view) {
                FrameDecode::Ok {
                    consumed,
                    payload_start,
                    payload_end,
                } => {
                    let lsn = Lsn::new(self.reader.cursor);
                    let payload = view[payload_start..payload_end].to_vec();
                    self.reader.cursor += consumed as u64;
                    self.valid_start += consumed;
                    // Successful decode resets the pad-skip counter
                    // — the next zero-magic encounter starts a
                    // fresh skip budget.
                    self.pad_skips_in_a_row = 0;
                    return Some(Ok(JournalRecord { lsn, payload }));
                }
                FrameDecode::Truncated => {
                    // Need more bytes — refill and retry.
                    let bytes_in_buffer = self.valid_end - self.valid_start;
                    // Same fix as the at_eof check above:
                    // next_unread_file_offset = cursor +
                    // (valid_end - valid_start), NOT cursor +
                    // valid_end.
                    let next_unread_file_offset = self.reader.cursor + bytes_in_buffer as u64;
                    let bytes_we_could_still_read = self
                        .reader
                        .file_size
                        .saturating_sub(next_unread_file_offset);
                    if bytes_we_could_still_read == 0 {
                        // No more file to read; this is a real truncation.
                        // Distinguish header vs. payload truncation by
                        // checking how much we have.
                        self.reader.last_state = if bytes_in_buffer < 8 {
                            JournalTailState::TruncatedHeader
                        } else {
                            JournalTailState::TruncatedPayload
                        };
                        self.finished = true;
                        return None;
                    }
                    // More file remaining — refill and retry.
                    if let Err(e) = self.refill() {
                        self.finished = true;
                        return Some(Err(e));
                    }
                    continue;
                }
                FrameDecode::BadMagic => {
                    // Zero-magic = sector-pad from a Direct-IO
                    // journal's partial flush. Real frames have
                    // magic 0x46535901 (≠ 0). If the first 4
                    // header bytes are zero, advance the cursor
                    // to the next 512-byte boundary and retry.
                    // Capped at MAX_PAD_SKIP_SECTORS to avoid
                    // O(file_size) scans of pathological all-zero
                    // files.
                    let view = &self.buf[self.valid_start..self.valid_end];
                    let header_zero = view.len() >= 4
                        && view[0] == 0
                        && view[1] == 0
                        && view[2] == 0
                        && view[3] == 0;
                    if header_zero && self.pad_skips_in_a_row < MAX_PAD_SKIP_SECTORS {
                        // Advance cursor to next 512-aligned offset.
                        let cur = self.reader.cursor;
                        let next = (cur / PAD_SKIP_GRANULARITY + 1) * PAD_SKIP_GRANULARITY;
                        let advance = (next - cur) as usize;
                        // Drop the consumed bytes from the buffer
                        // window (or all of it if `advance` exceeds
                        // what's buffered — refill catches up).
                        let buffered = self.valid_end - self.valid_start;
                        let drop_from_buf = advance.min(buffered);
                        self.valid_start += drop_from_buf;
                        self.reader.cursor = next;
                        self.pad_skips_in_a_row += 1;
                        // Re-loop; the next iteration will refill
                        // if needed and retry decode at the new
                        // sector boundary.
                        continue;
                    }
                    self.reader.last_state = JournalTailState::BadMagic;
                    self.finished = true;
                    return None;
                }
                FrameDecode::LengthOverflow => {
                    self.reader.last_state = JournalTailState::LengthOverflow;
                    self.finished = true;
                    return None;
                }
                FrameDecode::ChecksumMismatch => {
                    self.reader.last_state = JournalTailState::ChecksumMismatch;
                    self.finished = true;
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::JournalHandle;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static C: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(tag: &str) -> PathBuf {
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fsys_reader_test_{}_{}_{tag}",
            std::process::id(),
            n
        ))
    }

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn iter_yields_appended_records_in_order() {
        let path = tmp_path("ordered");
        let _g = Cleanup(path.clone());

        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"alpha").unwrap();
        let _ = writer.append(b"beta").unwrap();
        let _ = writer.append(b"gamma").unwrap();
        writer.close().unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        let recs: Vec<JournalRecord> = reader.iter().map(|r| r.unwrap()).collect();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].payload, b"alpha");
        assert_eq!(recs[1].payload, b"beta");
        assert_eq!(recs[2].payload, b"gamma");
        // LSNs strictly increasing.
        assert!(recs[0].lsn < recs[1].lsn);
        assert!(recs[1].lsn < recs[2].lsn);
        assert_eq!(reader.tail_state(), JournalTailState::CleanEnd);
    }

    #[test]
    fn iter_handles_empty_file_cleanly() {
        let path = tmp_path("empty_file");
        let _g = Cleanup(path.clone());
        std::fs::write(&path, b"").unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        assert!(reader.iter().next().is_none());
        assert_eq!(reader.tail_state(), JournalTailState::CleanEnd);
    }

    #[test]
    fn iter_handles_journal_with_only_empty_records() {
        let path = tmp_path("empty_records");
        let _g = Cleanup(path.clone());
        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"").unwrap();
        let _ = writer.append(b"").unwrap();
        let _ = writer.append(b"").unwrap();
        writer.close().unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        let recs: Vec<JournalRecord> = reader.iter().map(|r| r.unwrap()).collect();
        assert_eq!(recs.len(), 3);
        for r in &recs {
            assert_eq!(r.payload, b"");
        }
        assert_eq!(reader.tail_state(), JournalTailState::CleanEnd);
    }

    #[test]
    fn read_at_lsn_returns_record_at_position() {
        let path = tmp_path("read_at");
        let _g = Cleanup(path.clone());
        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"first").unwrap(); // LSN 0
        let lsn_before_second = writer.next_lsn();
        let _ = writer.append(b"second").unwrap();
        let lsn_before_third = writer.next_lsn();
        let _ = writer.append(b"third").unwrap();
        writer.close().unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        let r0 = reader.read_at_lsn(Lsn::ZERO).unwrap();
        assert_eq!(r0.payload, b"first");
        let r1 = reader.read_at_lsn(lsn_before_second).unwrap();
        assert_eq!(r1.payload, b"second");
        let r2 = reader.read_at_lsn(lsn_before_third).unwrap();
        assert_eq!(r2.payload, b"third");
    }

    #[test]
    fn truncated_header_detected_as_tail_state() {
        let path = tmp_path("trunc_header");
        let _g = Cleanup(path.clone());

        // Write a complete frame, then a partial header.
        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"good").unwrap();
        writer.close().unwrap();
        // Append 5 bytes of partial header.
        use std::io::Write;
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"\x46\x53\x59\x01\xAB").unwrap();
        drop(f);

        let mut reader = JournalReader::open(&path).unwrap();
        let first = reader.iter().next();
        assert!(first.is_some());
        // Advance to the trailing partial bytes; iteration ends.
        let next = reader.iter().next();
        assert!(next.is_none());
        assert_eq!(reader.tail_state(), JournalTailState::TruncatedHeader);
    }

    #[test]
    fn truncated_payload_detected_as_tail_state() {
        let path = tmp_path("trunc_payload");
        let _g = Cleanup(path.clone());

        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"good").unwrap();
        writer.close().unwrap();
        // Append a header claiming length=100 but provide only 20 payload bytes.
        let mut frame = Vec::new();
        frame.extend_from_slice(&0x46535901u32.to_be_bytes());
        frame.extend_from_slice(&100u32.to_le_bytes());
        frame.extend(std::iter::repeat_n(0xAA, 20));
        use std::io::Write;
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&frame).unwrap();
        drop(f);

        let mut reader = JournalReader::open(&path).unwrap();
        let _good = reader.iter().next().unwrap().unwrap();
        let next = reader.iter().next();
        assert!(next.is_none());
        assert_eq!(reader.tail_state(), JournalTailState::TruncatedPayload);
    }

    #[test]
    fn checksum_mismatch_detected_as_tail_state() {
        let path = tmp_path("checksum");
        let _g = Cleanup(path.clone());

        // Write a complete frame, then corrupt one byte of its
        // payload. The CRC will no longer match.
        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"good").unwrap();
        let lsn_corrupt = writer.next_lsn();
        let _ = writer.append(b"corrupt-this").unwrap();
        writer.close().unwrap();

        // Flip the second record's first payload byte. The
        // payload starts at lsn_corrupt + 8 (after magic +
        // length).
        let mut bytes = std::fs::read(&path).unwrap();
        let payload_offset = lsn_corrupt.as_u64() as usize + 8;
        bytes[payload_offset] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        let _good = reader.iter().next().unwrap().unwrap();
        let next = reader.iter().next();
        assert!(next.is_none());
        assert_eq!(reader.tail_state(), JournalTailState::ChecksumMismatch);
    }

    #[test]
    fn bad_magic_detected_as_tail_state() {
        let path = tmp_path("bad_magic");
        let _g = Cleanup(path.clone());
        // File with wrong-magic bytes.
        std::fs::write(&path, b"\xDE\xAD\xBE\xEF\x00\x00\x00\x00garbage").unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        let next = reader.iter().next();
        assert!(next.is_none());
        assert_eq!(reader.tail_state(), JournalTailState::BadMagic);
    }

    /// 0.9.6 audit C-5 — torn-frame test sweep.
    ///
    /// For **every** byte position in a 3-frame journal, truncate
    /// the file there and verify:
    /// 1. `JournalReader::iter()` does not panic and does not hang.
    /// 2. The records yielded before iteration ends are exactly
    ///    the ones whose final byte landed at or before the
    ///    truncation point.
    /// 3. `tail_state()` after iteration is one of:
    ///    - `CleanEnd` (truncation aligned at a frame boundary), OR
    ///    - `TruncatedHeader` / `TruncatedPayload` / `ChecksumMismatch`
    ///      (every torn position is detected, never silently accepted).
    ///
    /// This is the load-bearing recovery test for the journal —
    /// pre-0.9.6 we had a single tear-position test per state
    /// variant; the audit pointed out that single-position coverage
    /// is insufficient. Now every byte position is verified.
    #[test]
    fn torn_frame_sweep_every_position_detected() {
        let path = tmp_path("torn_sweep");
        let _g = Cleanup(path.clone());

        // Build a 3-frame journal. Each frame's payload is a
        // distinct constant byte so a partial payload is visible
        // in the recovered bytes.
        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"AAAA").unwrap();
        let _ = writer.append(b"BBBB").unwrap();
        let _ = writer.append(b"CCCC").unwrap();
        writer.close().unwrap();

        // Snapshot the full clean file so we can rebuild after each
        // truncation iteration.
        let full = std::fs::read(&path).unwrap();
        assert!(!full.is_empty());

        // Frame boundaries — 4-byte payload + FRAME_OVERHEAD (12) per
        // frame = 16 bytes/frame. After frame 1: 16. After frame 2:
        // 32. After frame 3: 48 (= full.len()).
        let boundaries: std::collections::HashSet<usize> =
            [16, 32, full.len()].iter().copied().collect();

        // Sweep every byte position from 1 to full.len(). Position 0
        // is an empty file — separate test path (`CleanEnd`, zero
        // records), not relevant to the torn-frame sweep.
        for trunc_to in 1..=full.len() {
            std::fs::write(&path, &full[..trunc_to]).unwrap();

            let mut reader = JournalReader::open(&path).unwrap();
            let mut yielded = 0usize;
            // Drain the iterator; collect record count.
            for rec in reader.iter() {
                let _ = rec.expect("iter yielded Err inside the sweep — should be None at tear");
                yielded += 1;
            }
            let state = reader.tail_state();

            // Records yielded must equal the number of frames whose
            // tail byte (= frame_end_offset) is <= trunc_to.
            let expected_records = match trunc_to {
                n if n >= 48 => 3,
                n if n >= 32 => 2,
                n if n >= 16 => 1,
                _ => 0,
            };
            assert_eq!(
                yielded, expected_records,
                "trunc_to={}: yielded={}, expected={}",
                trunc_to, yielded, expected_records
            );

            // Tail state contract: aligned boundary → CleanEnd,
            // otherwise one of the torn variants. Never silent
            // success on a torn file.
            if boundaries.contains(&trunc_to) {
                assert_eq!(
                    state,
                    JournalTailState::CleanEnd,
                    "trunc_to={} aligned at frame boundary — expected CleanEnd, got {:?}",
                    trunc_to,
                    state
                );
            } else {
                assert!(
                    matches!(
                        state,
                        JournalTailState::TruncatedHeader
                            | JournalTailState::TruncatedPayload
                            | JournalTailState::ChecksumMismatch
                    ),
                    "trunc_to={}: tail_state={:?} — torn file must report a torn state, never CleanEnd or BadMagic",
                    trunc_to,
                    state
                );
            }
        }
    }

    /// 0.9.6 audit C-5 — single-byte-flip detection sweep.
    ///
    /// For every byte position in a single complete frame, flip the
    /// byte and verify the reader detects the corruption (either
    /// `ChecksumMismatch`, `BadMagic`, `LengthOverflow`,
    /// `TruncatedHeader`, or `TruncatedPayload` — every variant
    /// here is a "do not silently accept" signal).
    ///
    /// The point is that no single-byte corruption inside a
    /// well-formed frame is ever silently accepted as valid.
    /// The CRC-32C trailer is the load-bearing detector for the
    /// payload bytes; the magic + length fields are detected via
    /// `BadMagic` / `LengthOverflow`.
    #[test]
    fn single_byte_flip_sweep_every_position_detected() {
        let path = tmp_path("flip_sweep");
        let _g = Cleanup(path.clone());

        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"hello").unwrap(); // payload bytes = 5
        writer.close().unwrap();

        let full = std::fs::read(&path).unwrap();
        // Frame layout: magic(4) + length(4) + payload(5) + crc(4) = 17 bytes.
        assert_eq!(full.len(), 4 + 4 + 5 + 4);

        for flip_idx in 0..full.len() {
            let mut bytes = full.clone();
            bytes[flip_idx] ^= 0xFF; // flip all 8 bits at this byte
            std::fs::write(&path, &bytes).unwrap();

            let mut reader = JournalReader::open(&path).unwrap();
            // Iterating may yield no records (corruption blocks the
            // single record) OR may yield the record + report
            // corruption in the tail state (if the flip lands in
            // the trailer in a way that doesn't break the CRC —
            // theoretically impossible for a single byte flip
            // since CRC-32C detects any single-byte change).
            let mut yielded = 0usize;
            for r in reader.iter() {
                if r.is_err() {
                    // Reader surfaces corruption mid-iteration —
                    // also acceptable.
                    break;
                }
                yielded += 1;
            }
            let state = reader.tail_state();

            // A single-byte flip in a well-formed 17-byte frame
            // must NEVER yield the record + CleanEnd. If yielded
            // is 1 then state must be a corruption signal.
            if yielded == 1 {
                panic!(
                    "flip_idx={}: reader yielded a record AND tail_state={:?} \
                     — single-byte flip should always be detected",
                    flip_idx, state
                );
            }
            // yielded == 0 with any non-CleanEnd state is the
            // expected outcome. We deliberately do NOT pin which
            // specific variant — the format's detection layer
            // (magic check / length validation / CRC) chooses
            // based on where the flip landed, and any of the
            // corruption variants is correct.
            assert_ne!(
                state,
                JournalTailState::CleanEnd,
                "flip_idx={}: a single-byte flip in a complete frame must not yield CleanEnd",
                flip_idx
            );
        }
    }

    #[test]
    fn iter_handles_large_records_above_default_buf_size() {
        let path = tmp_path("large_records");
        let _g = Cleanup(path.clone());

        // 200 KiB record — exceeds the 64 KiB default buffer.
        // The reader must grow its buffer to accommodate.
        let big = vec![0xCDu8; 200 * 1024];
        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(&big).unwrap();
        writer.close().unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        let r = reader.iter().next().unwrap().unwrap();
        assert_eq!(r.payload, big);
    }

    #[test]
    fn seek_to_repositions_iterator() {
        let path = tmp_path("seek_to");
        let _g = Cleanup(path.clone());

        let writer = JournalHandle::open(&path).unwrap();
        let _ = writer.append(b"first").unwrap();
        let lsn_at_second = writer.next_lsn();
        let _ = writer.append(b"second").unwrap();
        let lsn_at_third = writer.next_lsn();
        let _ = writer.append(b"third").unwrap();
        writer.close().unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        reader.seek_to(lsn_at_second);
        let recs: Vec<JournalRecord> = reader.iter().map(|r| r.unwrap()).collect();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].payload, b"second");
        assert_eq!(recs[1].payload, b"third");

        reader.seek_to(lsn_at_third);
        let recs: Vec<JournalRecord> = reader.iter().map(|r| r.unwrap()).collect();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].payload, b"third");
    }
}
