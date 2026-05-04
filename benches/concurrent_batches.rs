//! Group-lane throughput under concurrent producers.
//!
//! 16-thread concurrent batch submission against a single shared
//! handle. Each producer thread submits 16-op batches in a tight
//! loop. The benchmark measures the shared handle's group-lane
//! sustained throughput when contended.
//!
//! The 0.4.0 floor target is ≥ 100K ops/sec on consumer NVMe with
//! `Method::Direct`; ≥ 50K ops/sec with `Method::Sync` (validated on
//! the Linux primary box per D-3). Windows numbers are recorded as
//! the Windows-specific baseline for regression detection.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static C: AtomicU64 = AtomicU64::new(0);

fn bench_concurrent_batches(c: &mut Criterion) {
    let n_threads = 16usize;
    let ops_per_batch = 16usize;
    let payload = vec![0u8; 1024];
    let total_ops = (n_threads * ops_per_batch) as u64;

    let mut group = c.benchmark_group("concurrent_batches_16t_16op_1KiB");
    group.throughput(Throughput::Elements(total_ops));
    group.sample_size(20); // multi-thread benches are slower; smaller sample

    group.bench_function("sync_method", |b| {
        let handle = Arc::new(
            Builder::new()
                .method(Method::Sync)
                .build()
                .expect("build handle"),
        );

        b.iter_custom(|iters| {
            let start = std::time::Instant::now();
            for _ in 0..iters {
                let mut threads = Vec::new();
                for t in 0..n_threads {
                    let handle = Arc::clone(&handle);
                    let payload = payload.clone();
                    threads.push(std::thread::spawn(move || {
                        let n = C.fetch_add(1, Ordering::Relaxed);
                        let dir = std::env::temp_dir().join(format!(
                            "fsys_bench_conc_{}_{}_{}",
                            std::process::id(),
                            t,
                            n
                        ));
                        let _ = std::fs::create_dir_all(&dir);
                        let paths: Vec<PathBuf> = (0..ops_per_batch)
                            .map(|i| dir.join(format!("o{i}")))
                            .collect();
                        let batch: Vec<(&std::path::Path, &[u8])> = paths
                            .iter()
                            .map(|p| (p.as_path(), payload.as_slice()))
                            .collect();
                        handle.write_batch(&batch).expect("write_batch");
                        let _ = std::fs::remove_dir_all(&dir);
                    }));
                }
                for h in threads {
                    let _ = h.join();
                }
            }
            start.elapsed()
        });
    });

    group.finish();
}

criterion_group!(benches, bench_concurrent_batches);
criterion_main!(benches);
