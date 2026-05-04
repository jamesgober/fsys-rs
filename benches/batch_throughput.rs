//! Group-lane batch throughput at varying batch sizes.
//!
//! Measures `Handle::write_batch` end-to-end wall-clock time at batch
//! sizes 1, 16, 128, 1024 with a 1 KiB payload per op. The 0.4.0
//! floor target on Linux NVMe is < 5 ms for size 128 and < 30 ms for
//! size 1024. Windows numbers are recorded as the Windows-specific
//! baseline for regression detection (see decision D-3 in
//! `.dev/DECISIONS-0.4.0.md`).

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_dir() -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("fsys_bench_batch_{}_{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn bench_batch_throughput(c: &mut Criterion) {
    let payload = vec![0u8; 1024];
    let mut group = c.benchmark_group("batch_throughput_1KiB");

    for &n in &[1usize, 16, 128, 1024] {
        let handle = Builder::new()
            .method(Method::Sync)
            .build()
            .expect("build handle");
        let dir = tmp_dir();
        // Pre-allocate the path strings so the bench loop measures
        // dispatch + IO, not path construction.
        let paths: Vec<PathBuf> = (0..n).map(|i| dir.join(format!("op_{i}"))).collect();

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let batch: Vec<(&std::path::Path, &[u8])> = paths
                    .iter()
                    .map(|p| (p.as_path(), payload.as_slice()))
                    .collect();
                handle.write_batch(&batch).expect("write_batch");
            });
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    group.finish();
}

criterion_group!(benches, bench_batch_throughput);
criterion_main!(benches);
