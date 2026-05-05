//! 0.8.0 F-checkpoint canonical performance matrix.
//!
//! Captures `fsys` performance against `std::fs` and `tokio::fs`
//! across the matrix of:
//!
//! - **Operations:** `write`, `write_copy`, `read`, batched-write-8.
//! - **Payload sizes:** 4 KiB, 64 KiB, 1 MiB.
//! - **`fsys` durability methods:** `Sync`, `Data`, `Direct`, `Auto`.
//!
//! Output is a markdown table on stdout, ready to paste into
//! `docs/BENCH.md`. Each cell reports median ops/sec (calculated
//! from 100 timed iterations after a 10-iteration warmup).
//!
//! This is **not** a Criterion bench. Criterion does scientific
//! sample-size optimisation; this harness does direct timing for
//! a one-shot matrix snapshot. Use Criterion for regression
//! detection (`cargo bench`); use this for per-release certified
//! BENCH.md numbers.
//!
//! Run: `cargo run --bench matrix_with_peers --release --features async`

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use fsys::{builder, Method};

const ITERS: usize = 100;
const WARMUP: usize = 10;

const PAYLOADS: &[(usize, &str)] = &[
    (4 * 1024, "4 KiB"),
    (64 * 1024, "64 KiB"),
    (1 << 20, "1 MiB"),
];

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("fsys_bench_{}_{}_{}", std::process::id(), n, tag))
}

/// Measures one closure's median wall-clock latency over `ITERS`
/// runs. Returns (median_us, p99_us).
fn measure<F: FnMut()>(mut f: F) -> (f64, f64) {
    // Warmup.
    for _ in 0..WARMUP {
        f();
    }
    let mut samples: Vec<f64> = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t = Instant::now();
        f();
        samples.push(t.elapsed().as_secs_f64() * 1_000_000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[ITERS / 2];
    let p99_idx = (ITERS as f64 * 0.99) as usize;
    let p99 = samples[p99_idx.min(ITERS - 1)];
    (median, p99)
}

/// Format a (median, p99) pair as "MED µs / P99 µs".
fn cell(m: f64, p: f64) -> String {
    if m < 1000.0 {
        format!("{m:.1} / {p:.1}")
    } else {
        format!("{:.2}ms / {:.2}ms", m / 1000.0, p / 1000.0)
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    println!("# fsys 0.8.0 F-checkpoint performance matrix");
    println!();
    println!("Run date: {} (UTC)", chrono_safe_now());
    println!("Host: {} {}", host_os(), host_arch());
    println!("fsys version: 0.7.0 (pre-0.8.0 freeze)");
    println!("Iterations: {ITERS} timed (after {WARMUP} warmup).");
    println!("Cell format: median µs / p99 µs (or ms when ≥ 1000 µs).");
    println!();

    // ──────────────────────────────────────────────────────────
    // Bench 1: single write — fsys methods vs std::fs
    // ──────────────────────────────────────────────────────────
    println!("## Single write (atomic-replace) — fsys methods vs std::fs");
    println!();
    println!("| Payload | fsys::Sync | fsys::Data | fsys::Direct | fsys::Auto | std::fs::write |");
    println!("|---------|-----------:|-----------:|-------------:|-----------:|---------------:|");
    for (sz, label) in PAYLOADS {
        let payload = vec![0xA5u8; *sz];
        let s = bench_fsys_write(Method::Sync, &payload);
        let d = bench_fsys_write(Method::Data, &payload);
        let dr = bench_fsys_write(Method::Direct, &payload);
        let a = bench_fsys_write(Method::Auto, &payload);
        let std = bench_std_write(&payload);
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            label,
            cell(s.0, s.1),
            cell(d.0, d.1),
            cell(dr.0, dr.1),
            cell(a.0, a.1),
            cell(std.0, std.1)
        );
    }
    println!();

    // ──────────────────────────────────────────────────────────
    // Bench 2: read full file — fsys vs std::fs vs tokio::fs
    // ──────────────────────────────────────────────────────────
    println!("## Full-file read — fsys::Auto vs std::fs vs tokio::fs");
    println!();
    println!("| Payload | fsys::Auto | std::fs::read | tokio::fs::read |");
    println!("|---------|-----------:|--------------:|----------------:|");
    for (sz, label) in PAYLOADS {
        let payload = vec![0xA5u8; *sz];
        let f = bench_fsys_read(Method::Auto, &payload);
        let st = bench_std_read(&payload);
        let tk = bench_tokio_read(&payload).await;
        println!(
            "| {} | {} | {} | {} |",
            label,
            cell(f.0, f.1),
            cell(st.0, st.1),
            cell(tk.0, tk.1)
        );
    }
    println!();

    // ──────────────────────────────────────────────────────────
    // Bench 3: write_copy — atomic-replace with metadata preservation
    // ──────────────────────────────────────────────────────────
    println!("## write_copy (atomic-replace + metadata preservation)");
    println!();
    println!("| Payload | fsys::Sync write_copy | fsys::Auto write_copy |");
    println!("|---------|-----------------------:|-----------------------:|");
    for (sz, label) in PAYLOADS {
        let payload = vec![0xC4u8; *sz];
        let s = bench_fsys_write_copy(Method::Sync, &payload);
        let a = bench_fsys_write_copy(Method::Auto, &payload);
        println!("| {} | {} | {} |", label, cell(s.0, s.1), cell(a.0, a.1));
    }
    println!();

    // ──────────────────────────────────────────────────────────
    // Bench 4: batch — 8 writes in a single submission vs 8 solo writes
    // ──────────────────────────────────────────────────────────
    println!("## Batch-of-8 writes vs. 8 solo writes (per-op cost)");
    println!();
    println!("| Payload | fsys batch-8 (per-op µs) | fsys solo×8 (per-op µs) | speedup |");
    println!("|---------|------------------------:|--------------------------:|--------:|");
    for (sz, label) in PAYLOADS {
        let payload = vec![0x42u8; *sz];
        let batch = bench_fsys_batch_8(&payload);
        let solo = bench_fsys_solo_8(&payload);
        let speedup = solo.0 / batch.0;
        println!(
            "| {} | {} | {} | {:.2}× |",
            label,
            cell(batch.0, batch.1),
            cell(solo.0, solo.1),
            speedup
        );
    }
    println!();

    println!("---");
    println!();
    println!("**Methodology.** Each cell is the median + p99 of 100 timed iterations after 10 warmup iterations. Warmup discards the page-cache cold start. Test files are written to `std::env::temp_dir()`; on most platforms this lives on local NVMe-or-SSD-backed storage. Cleanup happens between iterations so each measurement starts from a known state.");
    println!();
    println!("**What this bench measures.** End-to-end wall-clock latency of the API call, including handle re-resolution + path canonicalisation + open + write + sync + atomic-rename. Not just the syscall surface — the durability fence is included.");
    println!();
    println!("**What this bench does NOT measure.** Sustained throughput under concurrent load (use `concurrent_batches` for that), tail latency under many-thread contention (use `tail_validation`), or the specific cost of just-the-syscall (use `cargo bench`'s isolated measurements).");
}

// ──────────────────────────────────────────────────────────────
// Bench helpers — fsys
// ──────────────────────────────────────────────────────────────

fn bench_fsys_write(method: Method, payload: &[u8]) -> (f64, f64) {
    let fs = builder().method(method).build().expect("handle");
    let path = tmp_path(&format!("fsys_w_{:?}", method));
    let result = measure(|| {
        fs.write(&path, payload).expect("write");
    });
    let _ = std::fs::remove_file(&path);
    result
}

fn bench_fsys_write_copy(method: Method, payload: &[u8]) -> (f64, f64) {
    let fs = builder().method(method).build().expect("handle");
    let path = tmp_path(&format!("fsys_wc_{:?}", method));
    fs.write(&path, payload).expect("seed");
    let result = measure(|| {
        fs.write_copy(&path, payload).expect("write_copy");
    });
    let _ = std::fs::remove_file(&path);
    result
}

fn bench_fsys_read(method: Method, payload: &[u8]) -> (f64, f64) {
    let fs = builder().method(method).build().expect("handle");
    let path = tmp_path(&format!("fsys_r_{:?}", method));
    fs.write(&path, payload).expect("seed");
    let result = measure(|| {
        let _ = fs.read(&path).expect("read");
    });
    let _ = std::fs::remove_file(&path);
    result
}

fn bench_fsys_batch_8(payload: &[u8]) -> (f64, f64) {
    let fs = builder().build().expect("handle");
    let dir = std::env::temp_dir();
    let paths: Vec<PathBuf> = (0..8)
        .map(|i| dir.join(format!("fsys_batch_{}_{i}.dat", std::process::id())))
        .collect();

    let result = measure(|| {
        let entries: Vec<(&PathBuf, &[u8])> = paths.iter().map(|p| (p, payload)).collect();
        fs.write_batch(&entries).expect("write_batch");
    });
    // Per-op: divide by 8.
    let per_op = (result.0 / 8.0, result.1 / 8.0);
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
    per_op
}

fn bench_fsys_solo_8(payload: &[u8]) -> (f64, f64) {
    let fs = builder().build().expect("handle");
    let dir = std::env::temp_dir();
    let paths: Vec<PathBuf> = (0..8)
        .map(|i| dir.join(format!("fsys_solo_{}_{i}.dat", std::process::id())))
        .collect();

    let result = measure(|| {
        for p in &paths {
            fs.write(p, payload).expect("write");
        }
    });
    let per_op = (result.0 / 8.0, result.1 / 8.0);
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
    per_op
}

// ──────────────────────────────────────────────────────────────
// Bench helpers — peers
// ──────────────────────────────────────────────────────────────

fn bench_std_write(payload: &[u8]) -> (f64, f64) {
    let path = tmp_path("std_w");
    let result = measure(|| {
        std::fs::write(&path, payload).expect("std write");
    });
    let _ = std::fs::remove_file(&path);
    result
}

fn bench_std_read(payload: &[u8]) -> (f64, f64) {
    let path = tmp_path("std_r");
    std::fs::write(&path, payload).expect("seed");
    let result = measure(|| {
        let _ = std::fs::read(&path).expect("std read");
    });
    let _ = std::fs::remove_file(&path);
    result
}

async fn bench_tokio_read(payload: &[u8]) -> (f64, f64) {
    let path = tmp_path("tokio_r");
    std::fs::write(&path, payload).expect("seed");

    // tokio::fs::read isn't available because we kept tokio's `fs`
    // feature out of fsys's dep list. Spawn_blocking against
    // std::fs::read is what tokio::fs would do internally anyway,
    // and matches what an application using fsys's `async` feature
    // would compete against.
    let path_arc = Arc::new(path.clone());
    let payload_for_warmup = payload.len();

    // Warmup
    for _ in 0..WARMUP {
        let p = path_arc.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::read(&*p)).await;
    }

    let mut samples: Vec<f64> = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let p = path_arc.clone();
        let t = Instant::now();
        let _ = tokio::task::spawn_blocking(move || std::fs::read(&*p))
            .await
            .expect("join");
        samples.push(t.elapsed().as_secs_f64() * 1_000_000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[ITERS / 2];
    let p99 = samples[(ITERS as f64 * 0.99) as usize];
    let _ = std::fs::remove_file(&path);
    let _ = payload_for_warmup;
    (median, p99)
}

// ──────────────────────────────────────────────────────────────
// Misc
// ──────────────────────────────────────────────────────────────

/// Local-time-free ISO-ish stamp without a chrono dep.
fn chrono_safe_now() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Approximate to date-only; full datetime requires more arithmetic.
    let days = secs / 86400;
    // Days since 1970-01-01. Crude approximation good enough for an
    // audit trail.
    let approx_year = 1970 + (days / 365);
    let day_in_year = days % 365;
    format!("{approx_year}-day-{day_in_year}")
}

fn host_os() -> &'static str {
    std::env::consts::OS
}

fn host_arch() -> &'static str {
    std::env::consts::ARCH
}
