//! Latency tail validation harness — confirms `p99.9 within 10× p50`
//! across representative ops.
//!
//! Per locked decision D-5 in `.dev/DECISIONS-0.7.0.md`, every
//! benchmark gets a relative-target check: the worst 0.1% of
//! operations should fall within 10× of the median. This is
//! portable across hardware (absolute numbers vary; ratios
//! don't).
//!
//! ## What this bench does
//!
//! Three representative ops, each measured 10 000 times to get
//! stable percentile estimates:
//!
//! 1. `Method::Sync` 4 KiB write — universal correctness floor.
//! 2. `Method::Direct` 4 KiB write — the latency-sensitive path.
//! 3. Read of an existing 4 KiB file — quick read-path sanity.
//!
//! Each op's latencies are sorted; the harness extracts p50,
//! p99, and p99.9, computes the ratio, and prints it. Failures
//! (ratio > 10) are LOGGED, not panicked — per D-5, "failures
//! are defects to investigate, not necessarily defects to fix."
//! Investigation is human-driven from the printed output.
//!
//! ## Sample size rationale
//!
//! 10 000 samples per op = 10 samples in the p99.9 bucket. That's
//! the minimum for a stable p99.9 estimate. Smaller samples
//! produce p99.9 noise > the 10× target; larger doesn't pay off.

use criterion::{criterion_group, criterion_main, Criterion};
use fsys::{builder, Method};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_tail_validation_{}_{}_{}.dat",
        std::process::id(),
        n,
        tag
    ))
}

const SAMPLE_COUNT: usize = 10_000;
const TAIL_RATIO_TARGET: f64 = 10.0;

/// Measure `op` `SAMPLE_COUNT` times. Return per-call durations
/// in microseconds, sorted ascending.
fn measure_sorted_us(mut op: impl FnMut()) -> Vec<f64> {
    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        let t0 = Instant::now();
        op();
        let dt = t0.elapsed();
        samples.push(dt.as_secs_f64() * 1e6);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    samples
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report_tail(name: &str, samples: &[f64]) {
    let p50 = percentile(samples, 50.0);
    let p99 = percentile(samples, 99.0);
    let p999 = percentile(samples, 99.9);
    let ratio = p999 / p50.max(0.001);
    let status = if ratio <= TAIL_RATIO_TARGET {
        "OK"
    } else {
        "INVESTIGATE"
    };
    eprintln!(
        "[tail_validation] {name}: p50={p50:.2}us p99={p99:.2}us p99.9={p999:.2}us ratio={ratio:.2}x [{status}]"
    );
    // Per D-5: failures are not panicked — they're logged for
    // human investigation. The bench passes either way.
}

fn bench_tail_sync_write(c: &mut Criterion) {
    let fs = builder().method(Method::Sync).build().expect("handle");
    let payload = vec![0xA5u8; 4096];

    // Stable temp path so we don't get filesystem-newdir cost
    // dominating the measurement.
    let path = tmp_path("sync_write");

    let samples = measure_sorted_us(|| {
        fs.write(&path, &payload).expect("write");
    });
    report_tail("sync_write_4k", &samples);
    let _ = std::fs::remove_file(&path);

    // Single token Criterion bench so this file shows up in
    // `cargo bench --no-run` validation.
    c.bench_function("tail_validation_anchor", |b| {
        b.iter(|| 1u64 + 1u64);
    });
}

fn bench_tail_direct_write(c: &mut Criterion) {
    let fs = builder().method(Method::Direct).build().expect("handle");
    let payload = vec![0xC3u8; 4096];
    let path = tmp_path("direct_write");

    let samples = measure_sorted_us(|| {
        let _ = fs.write(&path, &payload);
    });
    report_tail("direct_write_4k", &samples);
    let _ = std::fs::remove_file(&path);

    // Filler bench — see comment in bench_tail_sync_write.
    c.bench_function("tail_validation_anchor_direct", |b| {
        b.iter(|| 2u64 + 2u64);
    });
}

fn bench_tail_read(c: &mut Criterion) {
    let path = tmp_path("read");
    std::fs::write(&path, vec![0u8; 4096]).expect("setup");
    let fs = builder().build().expect("handle");

    let samples = measure_sorted_us(|| {
        let _ = fs.read(&path);
    });
    report_tail("read_4k", &samples);
    let _ = std::fs::remove_file(&path);

    c.bench_function("tail_validation_anchor_read", |b| {
        b.iter(|| 3u64 + 3u64);
    });
}

criterion_group!(
    benches,
    bench_tail_sync_write,
    bench_tail_direct_write,
    bench_tail_read
);
criterion_main!(benches);
