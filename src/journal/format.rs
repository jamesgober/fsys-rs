//! Journal record framing format — self-identifying, integrity-
//! protected, tail-truncation-safe.
//!
//! Every record written by [`JournalHandle::append`](super::JournalHandle::append)
//! is wrapped in a 12-byte frame:
//!
//! ```text
//!  bytes  field           description
//!  -----  -----           -----------
//!  0..4   magic_and_ver   big-endian u32: 0x46535901 ("FSY\x01")
//!                         — identifies the file as an fsys journal
//!                         v1 stream.
//!  4..8   length          little-endian u32: payload byte length.
//!  8..    payload         exactly `length` bytes of caller payload.
//!  ..+4   crc32c          little-endian u32: crc32c of the
//!                         preceding (magic_and_ver + length +
//!                         payload). Validates the entire frame.
//! ```
//!
//! Total frame overhead: **12 bytes per record** (4 magic+ver +
//! 4 length + 4 crc32c). At 64-byte records that's 19% overhead;
//! at 4 KiB it's 0.3%; at 64 KiB it's 0.02%. The overhead is
//! constant per record, so larger records amortise the framing
//! to negligible cost.
//!
//! ## Why these specific choices
//!
//! - **Magic prefix:** lets a reader detect immediately whether
//!   the file at a path is actually an fsys journal vs. a raw
//!   file vs. a different format. Catches format-confusion
//!   attacks (a file with the right extension but the wrong
//!   contents) before they corrupt downstream logic.
//! - **Version byte in the magic:** allows the on-disk format to
//!   evolve without breaking forward compatibility. v2/v3 frames
//!   would set the low byte to 0x02 / 0x03; readers reject
//!   unknown versions explicitly.
//! - **CRC32C** (Castagnoli polynomial) over **xxhash / SHA / etc:**
//!   crc32c is hardware-accelerated on every x86 CPU since 2008
//!   (via the SSE4.2 `crc32` instruction) and on every ARMv8.1+
//!   CPU. On hardware where it's accelerated, crc32c is the
//!   fastest checksum available — typically 1 cycle per 8 bytes.
//!   Checksumming a 4 KiB record costs ~500 ns on modern x86.
//!   xxhash64 would be faster on hardware *without* crc32c
//!   acceleration, but for the database-WAL target the user-
//!   facing platforms (modern Linux x86-64 / ARM64 servers) all
//!   have crc32c hardware support.
//! - **Trailing checksum** vs. inline header checksum: the
//!   checksum is at the end so the writer can compute it from
//!   the already-buffered preceding bytes without re-reading.
//!   For journals with very large records this matters; for
//!   small records it's wash.
//! - **Forward-iteration only:** the format does not include a
//!   length trailer that would enable backward iteration.
//!   Forward replay is the canonical WAL pattern (replay from
//!   last checkpoint to the end). A reverse iteration would
//!   need length-trailer plumbing — filed as 0.9.x if a real
//!   user need surfaces.
//!
//! ## Tail-truncation detection
//!
//! On a crash mid-write, the journal file may end with a
//! partially written frame. The reader detects this in one of
//! three ways:
//!
//! 1. **Truncated header:** less than 8 bytes left when starting
//!    a new frame → end-of-journal.
//! 2. **Truncated payload:** length field claims N bytes but
//!    the file ends before N bytes are read → end-of-journal.
//! 3. **Truncated checksum or checksum mismatch:** the trailing
//!    crc32c is missing or doesn't match the computed value →
//!    end-of-journal.
//!
//! In all three cases the reader stops cleanly at the last
//! fully-written record. Partial bytes after that point are
//! discarded (not returned to the caller), and the resume
//! cursor for a new [`JournalHandle`] should be set to the LSN
//! at which the partial bytes started — the writer will
//! overwrite them.

#![allow(dead_code)] // some helpers are reserved for the writer-side path

use crate::{Error, Result};

/// Frame magic + format version. Big-endian on disk so a
/// hexdump shows `46 53 59 01` clearly.
///
/// `0x46535901`:
/// - `0x46 0x53 0x59` = `"FSY"` (the same prefix as the
///   `fsys::primitive` constants and the `FS-NNNNN` error codes).
/// - `0x01` = format version. Future v2 would be `0x46535902`.
pub(crate) const FRAME_MAGIC_V1: u32 = 0x4653_5901;

/// Frame overhead: magic_and_ver (4) + length (4) + crc32c (4) = 12 bytes.
pub(crate) const FRAME_OVERHEAD: usize = 12;

/// Maximum record payload length supported by the v1 frame
/// format. The length field is `u32`, but we cap below
/// `u32::MAX` to leave headroom for future flag-bit semantics
/// in the high bits.
///
/// Records larger than this should be sharded by the caller
/// (split across multiple appends with an application-level
/// sequencing scheme). For typical WAL workloads (record sizes
/// 64 B – 4 KiB) this cap is unreachable.
pub(crate) const FRAME_MAX_PAYLOAD: u32 = (1 << 28) - 1; // 256 MiB

// ─────────────────────────────────────────────────────────────────
// CRC32C (Castagnoli polynomial, used by SCSI / iSCSI / Btrfs /
// every database WAL since the late 2000s).
//
// Implementation: software lookup table fallback. On modern x86 +
// ARM the Rust compiler with `-C target-feature=+sse4.2` /
// `+crc` would emit hardware crc32 instructions, but we don't
// hard-require those features (MSRV 1.75 + cross-platform
// compatibility). Software crc32c via a precomputed lookup table
// is ~2 GB/s on a single core — fast enough that checksumming
// is not a bottleneck on the journal hot path even for 1 MiB
// records.
//
// The lookup-table entries are computed at compile time so we
// pay zero startup cost; the table fits in 1 KiB of read-only
// data.
// ─────────────────────────────────────────────────────────────────

/// Castagnoli (CRC-32C) polynomial reflected:
/// `x^32 + x^28 + x^27 + x^26 + x^25 + x^23 + x^22 + x^20 + x^19 + x^18 + x^14 + x^13 + x^11 + x^10 + x^9 + x^8 + x^6 + 1`
/// reflected → `0x82F63B78`.
const CRC32C_POLY: u32 = 0x82F6_3B78;

/// Precomputed 256-entry CRC-32C lookup table.
const CRC32C_TABLE: [u32; 256] = build_crc32c_table();

const fn build_crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLY
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Computes the CRC-32C checksum of `bytes`, starting from the
/// initial value `0xFFFF_FFFF` and finalising with a bitwise NOT
/// (the conventional CRC-32C protocol per RFC 3720).
#[inline]
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc = (crc >> 8) ^ CRC32C_TABLE[((crc ^ b as u32) & 0xFF) as usize];
    }
    !crc
}

/// Streaming CRC-32C — useful when checksumming is split across
/// multiple buffers (header bytes + payload bytes) without
/// allocating a contiguous combined buffer.
///
/// Call [`Crc32cBuilder::new`] to start; feed bytes via
/// [`Crc32cBuilder::update`]; finalise with
/// [`Crc32cBuilder::finalize`].
pub(crate) struct Crc32cBuilder {
    crc: u32,
}

impl Crc32cBuilder {
    /// Begins a new CRC computation.
    #[inline]
    pub(crate) fn new() -> Self {
        Self { crc: 0xFFFF_FFFF }
    }

    /// Feeds `bytes` into the computation.
    #[inline]
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.crc;
        for &b in bytes {
            crc = (crc >> 8) ^ CRC32C_TABLE[((crc ^ b as u32) & 0xFF) as usize];
        }
        self.crc = crc;
    }

    /// Returns the finalised CRC-32C value (post bitwise NOT).
    #[inline]
    pub(crate) fn finalize(self) -> u32 {
        !self.crc
    }
}

// ─────────────────────────────────────────────────────────────────
// Frame encoding / decoding helpers
// ─────────────────────────────────────────────────────────────────

/// Encodes a frame for `payload` into `buf` (which must be sized
/// to `FRAME_OVERHEAD + payload.len()`).
///
/// On success, returns the number of bytes written (= buf len).
#[inline]
pub(crate) fn encode_frame_into(payload: &[u8], buf: &mut [u8]) -> Result<usize> {
    let total = payload.len().saturating_add(FRAME_OVERHEAD);
    if buf.len() < total {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "encode_frame_into: buffer too small",
        )));
    }
    if (payload.len() as u64) > (FRAME_MAX_PAYLOAD as u64) {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "journal record exceeds FRAME_MAX_PAYLOAD (256 MiB)",
        )));
    }
    let len = payload.len() as u32;

    // magic_and_ver — big-endian.
    buf[0..4].copy_from_slice(&FRAME_MAGIC_V1.to_be_bytes());
    // length — little-endian.
    buf[4..8].copy_from_slice(&len.to_le_bytes());
    // payload.
    buf[8..8 + payload.len()].copy_from_slice(payload);

    // crc32c over magic_and_ver + length + payload (everything
    // before the trailing checksum).
    let crc = crc32c(&buf[..8 + payload.len()]);
    buf[8 + payload.len()..total].copy_from_slice(&crc.to_le_bytes());

    Ok(total)
}

/// Allocates a fresh `Vec<u8>` containing the encoded frame for
/// `payload`. Convenience wrapper over [`encode_frame_into`] when
/// the caller doesn't already own a buffer.
#[inline]
pub(crate) fn encode_frame_owned(payload: &[u8]) -> Result<Vec<u8>> {
    let total = payload.len().saturating_add(FRAME_OVERHEAD);
    let mut buf = vec![0u8; total];
    let _ = encode_frame_into(payload, &mut buf)?;
    Ok(buf)
}

/// Outcome of attempting to decode a frame from a byte slice.
#[derive(Debug)]
pub(crate) enum FrameDecode {
    /// Successfully decoded a frame.
    Ok {
        /// Total bytes consumed (= [`FRAME_OVERHEAD`] + payload length).
        consumed: usize,
        /// Payload byte range within the input slice.
        payload_start: usize,
        payload_end: usize,
    },
    /// The input is too short to contain a complete frame —
    /// either the header is truncated or the payload is.
    /// The reader treats this as end-of-journal (tail truncation).
    Truncated,
    /// The frame's magic doesn't match — either this isn't an
    /// fsys journal, or the file is corrupted at this offset.
    BadMagic,
    /// The frame's length field exceeds [`FRAME_MAX_PAYLOAD`].
    LengthOverflow,
    /// The trailing CRC-32C doesn't match the computed value —
    /// the record's bytes are corrupt. The reader treats this
    /// as end-of-journal (tail truncation from a partial
    /// write).
    ChecksumMismatch,
}

/// Attempts to decode a frame starting at offset 0 of `bytes`.
///
/// `bytes` is typically a slice of a larger buffer; the decoder
/// reads only what it needs. The caller advances by `consumed`
/// bytes after a successful decode.
#[inline]
pub(crate) fn decode_frame(bytes: &[u8]) -> FrameDecode {
    if bytes.len() < 8 {
        return FrameDecode::Truncated;
    }
    let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic != FRAME_MAGIC_V1 {
        return FrameDecode::BadMagic;
    }
    let length = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if length > FRAME_MAX_PAYLOAD {
        return FrameDecode::LengthOverflow;
    }
    let length = length as usize;
    let total = 8 + length + 4; // header + payload + crc
    if bytes.len() < total {
        return FrameDecode::Truncated;
    }
    // Compute CRC-32C over the header + payload region.
    let computed = crc32c(&bytes[..8 + length]);
    let stored_crc = u32::from_le_bytes([
        bytes[8 + length],
        bytes[8 + length + 1],
        bytes[8 + length + 2],
        bytes[8 + length + 3],
    ]);
    if computed != stored_crc {
        return FrameDecode::ChecksumMismatch;
    }
    FrameDecode::Ok {
        consumed: total,
        payload_start: 8,
        payload_end: 8 + length,
    }
}

// ─────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-answer test vectors for CRC-32C, taken from RFC 3720
    /// and Castagnoli's original paper. If these fail, the
    /// implementation is wrong.
    #[test]
    fn crc32c_known_answer_vectors() {
        // Empty input → CRC-32C = 0x00000000 (initial 0xFFFFFFFF
        // post-NOT = 0).
        assert_eq!(crc32c(b""), 0);

        // 32 bytes of 0x00 → 0x8a9136aa per RFC 3720.
        let zeros = [0u8; 32];
        assert_eq!(crc32c(&zeros), 0x8a9136aa);

        // 32 bytes of 0xff → 0x62a8ab43 per RFC 3720.
        let ones = [0xffu8; 32];
        assert_eq!(crc32c(&ones), 0x62a8ab43);

        // Sequential bytes 0..32 → 0x46dd794e per RFC 3720.
        let seq: [u8; 32] = std::array::from_fn(|i| i as u8);
        assert_eq!(crc32c(&seq), 0x46dd794e);
    }

    #[test]
    fn streaming_crc_matches_one_shot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let one_shot = crc32c(data);

        let mut builder = Crc32cBuilder::new();
        builder.update(&data[..10]);
        builder.update(&data[10..25]);
        builder.update(&data[25..]);
        let streamed = builder.finalize();

        assert_eq!(one_shot, streamed);
    }

    #[test]
    fn frame_roundtrip_simple() {
        let payload = b"hello, journal!";
        let buf = encode_frame_owned(payload).expect("encode");
        assert_eq!(buf.len(), FRAME_OVERHEAD + payload.len());

        match decode_frame(&buf) {
            FrameDecode::Ok {
                consumed,
                payload_start,
                payload_end,
            } => {
                assert_eq!(consumed, buf.len());
                assert_eq!(&buf[payload_start..payload_end], payload);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn frame_roundtrip_empty_payload() {
        let buf = encode_frame_owned(b"").expect("encode");
        assert_eq!(buf.len(), FRAME_OVERHEAD);
        match decode_frame(&buf) {
            FrameDecode::Ok {
                payload_start,
                payload_end,
                ..
            } => {
                assert_eq!(payload_start, payload_end);
            }
            other => panic!("expected Ok empty, got {other:?}"),
        }
    }

    #[test]
    fn frame_roundtrip_4kib() {
        let payload = vec![0xCDu8; 4096];
        let buf = encode_frame_owned(&payload).expect("encode");
        match decode_frame(&buf) {
            FrameDecode::Ok {
                payload_start,
                payload_end,
                ..
            } => {
                assert_eq!(&buf[payload_start..payload_end], payload.as_slice());
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn truncated_header_returns_truncated() {
        let buf = [0u8; 5]; // < 8
        assert!(matches!(decode_frame(&buf), FrameDecode::Truncated));
    }

    #[test]
    fn truncated_payload_returns_truncated() {
        let payload = b"this won't fit";
        let mut buf = encode_frame_owned(payload).expect("encode");
        buf.truncate(buf.len() - 5); // chop off CRC + last few bytes of payload
        assert!(matches!(decode_frame(&buf), FrameDecode::Truncated));
    }

    #[test]
    fn bad_magic_returns_bad_magic() {
        let mut buf = encode_frame_owned(b"hi").expect("encode");
        buf[0] = 0xDE; // corrupt magic
        assert!(matches!(decode_frame(&buf), FrameDecode::BadMagic));
    }

    #[test]
    fn checksum_mismatch_returns_mismatch() {
        let mut buf = encode_frame_owned(b"original").expect("encode");
        // Flip a payload byte without updating the CRC.
        buf[8] ^= 0xFF;
        assert!(matches!(decode_frame(&buf), FrameDecode::ChecksumMismatch));
    }

    #[test]
    fn length_overflow_rejected_at_encode() {
        // We can't construct a Vec of FRAME_MAX_PAYLOAD+1 bytes
        // in a unit test economically. Instead, we synthesise a
        // header with the bad length and confirm the decoder
        // catches it.
        let mut buf = vec![0u8; 8];
        buf[0..4].copy_from_slice(&FRAME_MAGIC_V1.to_be_bytes());
        let bad_len = (FRAME_MAX_PAYLOAD + 1).to_le_bytes();
        buf[4..8].copy_from_slice(&bad_len);
        assert!(matches!(decode_frame(&buf), FrameDecode::LengthOverflow));
    }

    #[test]
    fn frame_overhead_is_exactly_12_bytes() {
        // Pinning this — the BENCH.md numbers depend on it being 12.
        assert_eq!(FRAME_OVERHEAD, 12);
    }

    #[test]
    fn frame_max_payload_fits_in_u32() {
        assert!((FRAME_MAX_PAYLOAD as u64) < (u32::MAX as u64));
    }

    /// Property test: 10,000 random round-trips. Covers payload
    /// sizes from empty to 64 KiB with random content. If the
    /// encoder/decoder ever disagrees, this test catches it
    /// statistically.
    ///
    /// Uses a deterministic linear-congruential PRNG seeded from
    /// the payload-length cursor so the test is reproducible.
    #[test]
    fn property_random_round_trips() {
        // Tiny LCG (numerical recipes constants) so the test is
        // self-contained and reproducible — no proptest dep
        // needed for a property check this simple.
        let mut state: u64 = 0xCAFEBABE_DEADBEEF;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };

        for iteration in 0..10_000u32 {
            // Vary payload size across iterations: power of 2
            // distribution from 0 to 16 KiB, with occasional
            // larger sizes up to 64 KiB.
            let size_class = (next() & 0xFF) as usize;
            let size = match size_class {
                0..=10 => 0,                                  // empty
                11..=50 => (next() % 64) as usize,            // tiny
                51..=150 => (next() % 1024) as usize,         // small
                151..=220 => (next() % (16 * 1024)) as usize, // medium
                _ => (next() % (64 * 1024)) as usize,         // large
            };
            // Fill payload with a deterministic but iteration-
            // sensitive byte pattern (xor with iteration so
            // adjacent iterations don't have identical content).
            let payload: Vec<u8> = (0..size)
                .map(|i| ((next() >> 16) as u8) ^ (iteration as u8) ^ (i as u8))
                .collect();
            let encoded = encode_frame_owned(&payload).expect("encode");
            assert_eq!(
                encoded.len(),
                FRAME_OVERHEAD + payload.len(),
                "encoded length wrong on iteration {iteration} (size {size})"
            );

            match decode_frame(&encoded) {
                FrameDecode::Ok {
                    consumed,
                    payload_start,
                    payload_end,
                } => {
                    assert_eq!(
                        consumed,
                        encoded.len(),
                        "consumed mismatch on iteration {iteration}"
                    );
                    assert_eq!(
                        &encoded[payload_start..payload_end],
                        payload.as_slice(),
                        "payload mismatch on iteration {iteration} (size {size})"
                    );
                }
                other => panic!("decode failed on iteration {iteration} (size {size}): {other:?}"),
            }
        }
    }

    /// Property test: every single-bit flip in any non-empty
    /// frame must surface as `ChecksumMismatch` or `BadMagic`
    /// — never as `Ok` with a different payload.
    ///
    /// CRC-32C provides single-bit-flip detection by
    /// construction (the polynomial is chosen for this); this
    /// test pins the property empirically.
    #[test]
    fn property_single_bit_flip_detected() {
        let payloads: &[&[u8]] = &[
            b"a",
            b"hello",
            b"the quick brown fox jumps over the lazy dog",
            &[0xFF; 256],
            &[0; 1024],
        ];
        for payload in payloads {
            let encoded = encode_frame_owned(payload).expect("encode");
            for byte_idx in 0..encoded.len() {
                for bit in 0..8 {
                    let mut corrupted = encoded.clone();
                    corrupted[byte_idx] ^= 1 << bit;
                    match decode_frame(&corrupted) {
                        FrameDecode::Ok {
                            payload_start,
                            payload_end,
                            ..
                        } if &corrupted[payload_start..payload_end] == *payload => {
                            panic!(
                                "single bit flip at byte {byte_idx} bit {bit} \
                                 produced a 'valid' decode with the original payload — \
                                 CRC-32C failed to detect corruption"
                            );
                        }
                        FrameDecode::Ok {
                            payload_start,
                            payload_end,
                            ..
                        } => {
                            // A different payload bytes that decoded
                            // — also a CRC failure (CRC must catch
                            // any single-bit flip).
                            panic!(
                                "single bit flip at byte {byte_idx} bit {bit} \
                                 produced a 'valid' decode with payload {:?}",
                                &corrupted[payload_start..payload_end]
                            );
                        }
                        // BadMagic / Truncated / LengthOverflow /
                        // ChecksumMismatch are all valid outcomes —
                        // the corruption was detected.
                        _ => {}
                    }
                }
            }
        }
    }

    #[test]
    fn forward_iteration_over_concatenated_frames() {
        // The reader's iter pattern: decode a frame, advance by
        // `consumed`, decode the next frame. This test pins the
        // invariant that consecutive frames decode cleanly.
        let mut all = Vec::new();
        let payloads: &[&[u8]] = &[b"first", b"second record", b"", b"fourth!"];
        for p in payloads {
            all.extend_from_slice(&encode_frame_owned(p).unwrap());
        }

        let mut cursor = 0;
        let mut decoded: Vec<&[u8]> = Vec::new();
        while cursor < all.len() {
            match decode_frame(&all[cursor..]) {
                FrameDecode::Ok {
                    consumed,
                    payload_start,
                    payload_end,
                } => {
                    decoded.push(&all[cursor + payload_start..cursor + payload_end]);
                    cursor += consumed;
                }
                other => panic!("frame {} unexpected: {other:?}", decoded.len()),
            }
        }
        assert_eq!(decoded, payloads);
    }
}
