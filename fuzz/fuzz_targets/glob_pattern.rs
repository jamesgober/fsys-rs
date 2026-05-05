#![no_main]
//! Fuzz target: `Handle::find` glob pattern parsing should reject
//! invalid patterns with a structured error rather than panicking.
//! The brace-expansion preprocessor is the most likely failure
//! surface (recursion + comma-splitting on adversarial input).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(pattern) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(fs) = fsys::builder().build() else {
        return;
    };
    // Run `find` against the temp dir with the fuzz pattern. We
    // don't care about the result — only that no panic, no
    // unbounded recursion, no integer overflow occurs. Errors are
    // expected outcomes for adversarial input.
    let _ = fs.find(std::env::temp_dir(), pattern);
});
