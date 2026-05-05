#![no_main]
//! Fuzz target: path normalization should never panic on any byte
//! sequence. The function exists to defang user-supplied paths
//! before they hit the filesystem layer; if it can crash on input
//! we'd rather find out via fuzz than via a production user.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    // Path normalization is the public surface we're fuzzing.
    let _ = fsys::path::normalize(s);
    // Segment sanitisation: same family of inputs.
    let _ = fsys::path::sanitize_segment(s);
});
