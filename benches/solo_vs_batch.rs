//! Solo lane vs single-op group lane: routing-decision verification.
//!
//! Per decision #2, routing is explicit: `Handle::write` always uses
//! the solo lane (zero pipeline overhead) and `Handle::write_batch`
//! always uses the group lane (queue + dispatcher). A single-op
//! `write_batch` *should* be measurably slower than the equivalent
//! solo `write` because it pays the dispatcher's queue + response-
//! channel round-trip.
//!
//! This benchmark validates that prediction. If the gap closes (e.g.
//! the solo lane regresses, or the group lane optimises away its
//! routing cost), the explicit-routing decision is no longer
//! defensible and we should re-evaluate.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(suffix: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_bench_solo_vs_batch_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ))
}

fn bench_solo_vs_batch(c: &mut Criterion) {
    let handle = Builder::new()
        .method(Method::Sync)
        .build()
        .expect("build handle");
    let payload = vec![0u8; 1024];

    let mut group = c.benchmark_group("solo_vs_batch_1op_1KiB");
    group.throughput(Throughput::Elements(1));

    let solo_path = tmp_path("solo");
    group.bench_function("solo_write", |b| {
        b.iter(|| {
            handle.write(&solo_path, &payload).expect("solo write");
        });
    });
    let _ = std::fs::remove_file(&solo_path);

    let batch_path = tmp_path("batch");
    group.bench_function("group_write_batch_1op", |b| {
        b.iter(|| {
            handle
                .write_batch(&[(batch_path.as_path(), payload.as_slice())])
                .expect("group write");
        });
    });
    let _ = std::fs::remove_file(&batch_path);

    group.finish();
}

criterion_group!(benches, bench_solo_vs_batch);
criterion_main!(benches);
