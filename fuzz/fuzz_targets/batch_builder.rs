#![no_main]
//! Fuzz target: the `Batch` builder API accumulates ops via
//! chainable `write` / `delete` / `copy`. The fuzz input is
//! interpreted as a sequence of (op-type, payload) pairs; the
//! builder must never panic regardless of the sequence's shape.
//!
//! We do NOT call `commit()` — that would actually touch the
//! filesystem. The fuzz target validates the BUILDER, not the
//! dispatcher.

use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;

fuzz_target!(|data: &[u8]| {
    let Ok(fs) = fsys::builder().build() else {
        return;
    };
    let mut batch = fs.batch();

    // Each input byte cycles through op types and produces a
    // synthetic path/payload. Cap the total ops to avoid
    // unbounded memory growth on huge fuzz corpora.
    let cap = data.len().min(1024);
    for (i, &b) in data.iter().take(cap).enumerate() {
        let path = PathBuf::from(format!("fuzz_op_{i}"));
        let payload = vec![b; (b as usize).min(64)];
        match b % 3 {
            0 => {
                batch.write(&path, &payload[..]);
            }
            1 => {
                batch.delete(&path);
            }
            _ => {
                let dst = PathBuf::from(format!("fuzz_op_{i}_dst"));
                batch.copy(&path, &dst);
            }
        }
    }
    // Drop without committing — confirms the builder's Drop is
    // also panic-safe.
});
