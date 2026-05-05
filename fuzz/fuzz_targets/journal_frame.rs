#![no_main]
//! Fuzz target: journal frame format must NEVER panic on
//! arbitrary input, and any successfully-decoded frame's payload
//! must be byte-equal to the encoded payload.
//!
//! ## Why this matters
//!
//! The journal frame format is the boundary between trusted
//! in-memory data and on-disk bytes that may have been corrupted
//! by hardware, power loss, or hostile operator. The decoder
//! receives arbitrary `&[u8]` slices and must:
//!
//! - Never panic — corruption must surface as `FrameDecode::Bad*`,
//!   not a process-killing assertion.
//! - Never accept a corrupt frame as valid — the CRC-32C check
//!   is what prevents a flipped bit from silently producing the
//!   wrong payload at the application layer.
//!
//! Per `.dev/DECISIONS-0.6.0.md` D-7, fuzz targets compile
//! against the public-or-pub-crate-test surface of the library
//! crate. The journal frame helpers are pub(crate), so this
//! target enables them via the `fuzz` cargo feature flag the
//! library exposes.
//!
//! ## What's checked per input
//!
//! 1. **Decoder never panics.** Calling `fsys::__fuzz::decode_frame`
//!    on the raw fuzzer input completes (any outcome is ok).
//! 2. **Encode → decode round-trip.** Constructing a frame from
//!    a bounded prefix of the input and decoding it must produce
//!    a frame whose payload bytes match the prefix.
//! 3. **Concatenation consistency.** Concatenating two encoded
//!    frames and forward-iterating decodes them in order to the
//!    original payloads.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Decoder must not panic on any byte sequence.
    let _ = fsys::__fuzz::decode_frame(data);

    // 2. Encode-decode round-trip.
    // Bound payload length so we don't OOM the fuzzer with a
    // pathological input. 64 KiB is well above typical WAL records.
    let payload_len = (data.len()).min(64 * 1024);
    let payload = &data[..payload_len];
    if let Ok(encoded) = fsys::__fuzz::encode_frame_owned(payload) {
        match fsys::__fuzz::decode_frame(&encoded) {
            fsys::__fuzz::FrameDecode::Ok {
                payload_start,
                payload_end,
                consumed,
            } => {
                assert_eq!(consumed, encoded.len(), "consumed != encoded len");
                assert_eq!(
                    &encoded[payload_start..payload_end],
                    payload,
                    "round-trip payload mismatch"
                );
            }
            other => panic!("encode produced un-decodable bytes: {other:?}"),
        }
    }

    // 3. Concatenation: split data into two halves, encode each,
    //    concatenate, forward-iterate decode → both decode in
    //    order with correct payloads.
    if data.len() >= 4 {
        let mid = data.len() / 2;
        let a = &data[..mid.min(32 * 1024)];
        let b = &data[mid..(mid + (data.len() - mid).min(32 * 1024))];
        let Ok(ea) = fsys::__fuzz::encode_frame_owned(a) else {
            return;
        };
        let Ok(eb) = fsys::__fuzz::encode_frame_owned(b) else {
            return;
        };
        let mut concat = Vec::with_capacity(ea.len() + eb.len());
        concat.extend_from_slice(&ea);
        concat.extend_from_slice(&eb);

        match fsys::__fuzz::decode_frame(&concat) {
            fsys::__fuzz::FrameDecode::Ok {
                consumed: ca,
                payload_start: ps_a,
                payload_end: pe_a,
            } => {
                assert_eq!(&concat[ps_a..pe_a], a, "first frame payload mismatch");
                match fsys::__fuzz::decode_frame(&concat[ca..]) {
                    fsys::__fuzz::FrameDecode::Ok {
                        consumed: _,
                        payload_start: ps_b,
                        payload_end: pe_b,
                    } => {
                        assert_eq!(
                            &concat[ca + ps_b..ca + pe_b],
                            b,
                            "second frame payload mismatch"
                        );
                    }
                    other => panic!("second frame decode failed: {other:?}"),
                }
            }
            other => panic!("first frame decode failed: {other:?}"),
        }
    }
});
