//! 0.5.0 method × payload × op matrix.
//!
//! Sweeps the four real durability methods (`Sync`, `Data`, `Direct`,
//! `Mmap`) across four payload sizes (4 KiB, 64 KiB, 1 MiB, 16 MiB)
//! and four operations (`write`, `read`, `write_batch_8`,
//! `read_after_write`). Per the 0.5.0 spec this is the canonical
//! per-release benchmark surface — the previous `method_comparison`
//! bench is retained for backward-compatible comparison points but is
//! covered (and superseded) by this matrix.
//!
//! All payloads are page-aligned so `Method::Mmap` exercises its real
//! mmap path (R-2'' in `.dev/DECISIONS-0.5.0.md`); sub-page payloads
//! would fall back to `Method::Sync` and the bench would measure the
//! wrong thing.
//!
//! `Method::Auto` is intentionally excluded from this matrix: it
//! resolves to one of the four real methods at handle construction,
//! so its numbers are exactly the resolved method's numbers — see
//! `method_comparison.rs` if you want an `Auto` data point on the
//! current host.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

const PAYLOADS: &[(usize, &str)] = &[
    (4 * 1024, "4KiB"),
    (64 * 1024, "64KiB"),
    (1024 * 1024, "1MiB"),
    (16 * 1024 * 1024, "16MiB"),
];

const METHODS: &[(Method, &str)] = &[
    (Method::Sync, "sync"),
    (Method::Data, "data"),
    (Method::Direct, "direct"),
    (Method::Mmap, "mmap"),
];

const BATCH_N: usize = 8;

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_bench_matrix_{}_{}_{}.dat",
        std::process::id(),
        n,
        tag
    ))
}

fn tmp_dir(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "fsys_bench_matrix_{}_{}_{}",
        std::process::id(),
        n,
        tag
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn bench_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("matrix_write");
    for &(method, m_tag) in METHODS {
        let handle = Builder::new().method(method).build().expect("build handle");
        for &(size, sz_tag) in PAYLOADS {
            let data = vec![0xA5u8; size];
            let path = tmp_path(&format!("{m_tag}_{sz_tag}_w"));
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::new(m_tag, sz_tag), &path, |b, p: &PathBuf| {
                b.iter(|| handle.write(p, &data).expect("write"));
            });
            let _ = std::fs::remove_file(&path);
        }
    }
    group.finish();
}

fn bench_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("matrix_read");
    for &(method, m_tag) in METHODS {
        let handle = Builder::new().method(method).build().expect("build handle");
        for &(size, sz_tag) in PAYLOADS {
            let data = vec![0x5Au8; size];
            let path = tmp_path(&format!("{m_tag}_{sz_tag}_r"));
            handle.write(&path, &data).expect("priming write");
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::new(m_tag, sz_tag), &path, |b, p: &PathBuf| {
                b.iter(|| {
                    let _ = handle.read(p).expect("read");
                });
            });
            let _ = std::fs::remove_file(&path);
        }
    }
    group.finish();
}

fn bench_write_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("matrix_write_batch_8");
    for &(method, m_tag) in METHODS {
        let handle = Builder::new().method(method).build().expect("build handle");
        for &(size, sz_tag) in PAYLOADS {
            // Skip the 16 MiB × batch_8 cell on the matrix: 128 MiB of
            // writes per Criterion sample × default 100-sample regimen
            // is ~10–15 minutes per cell on commodity NVMe and adds
            // little signal beyond what `matrix_write/16MiB` already
            // captures. Keep the four canonical sizes 4K..1M for the
            // batch op so the matrix completes in a reasonable
            // overall wall-clock.
            if size >= 16 * 1024 * 1024 {
                continue;
            }
            let data = vec![0x33u8; size];
            let dir = tmp_dir(&format!("{m_tag}_{sz_tag}_b"));
            let paths: Vec<PathBuf> = (0..BATCH_N).map(|i| dir.join(format!("op_{i}"))).collect();
            group.throughput(Throughput::Bytes((size * BATCH_N) as u64));
            group.bench_with_input(
                BenchmarkId::new(m_tag, sz_tag),
                &paths,
                |b, ps: &Vec<PathBuf>| {
                    b.iter(|| {
                        let batch: Vec<(&Path, &[u8])> =
                            ps.iter().map(|p| (p.as_path(), data.as_slice())).collect();
                        handle.write_batch(&batch).expect("write_batch");
                    });
                },
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    group.finish();
}

fn bench_read_after_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("matrix_read_after_write");
    for &(method, m_tag) in METHODS {
        let handle = Builder::new().method(method).build().expect("build handle");
        for &(size, sz_tag) in PAYLOADS {
            let data = vec![0xC3u8; size];
            let path = tmp_path(&format!("{m_tag}_{sz_tag}_rw"));
            group.throughput(Throughput::Bytes((size * 2) as u64));
            group.bench_with_input(BenchmarkId::new(m_tag, sz_tag), &path, |b, p: &PathBuf| {
                b.iter(|| {
                    handle.write(p, &data).expect("write");
                    let _ = handle.read(p).expect("read");
                });
            });
            let _ = std::fs::remove_file(&path);
        }
    }
    group.finish();
}

criterion_group!(
    matrix,
    bench_write,
    bench_read,
    bench_write_batch,
    bench_read_after_write
);
criterion_main!(matrix);
