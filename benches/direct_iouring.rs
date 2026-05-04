//! `Method::Direct` benchmark, with explicit io_uring documentation.
//!
//! ## Status of io_uring in 0.5.0
//!
//! Per the "io_uring 0.5.x blocker" section in
//! `.dev/DECISIONS-0.5.0.md`, the Linux io_uring submission path is
//! **stubbed** in 0.5.0 due to a rustc 1.95 ICE
//! (`check_mod_deathness` panic) when `io_uring::IoUring` is wrapped
//! in any std synchronisation primitive. `Method::Direct` on Linux
//! therefore currently runs the `O_DIRECT` + `pwrite` + `fdatasync`
//! fallback path. Numbers from this bench are the **post-stub
//! baseline** — re-run after the io_uring path is unstubbed in the
//! 0.5.x patch and compare to detect regression / measure the
//! io_uring uplift.
//!
//! On macOS this bench measures `F_NOCACHE` + `F_FULLFSYNC`. On
//! Windows it measures `FILE_FLAG_NO_BUFFERING` +
//! `FILE_FLAG_WRITE_THROUGH`. The `Method::Direct` contract is
//! identical across all three.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_bench_direct_{}_{}_{}.dat",
        std::process::id(),
        n,
        tag
    ))
}

fn bench_direct_writes(c: &mut Criterion) {
    let mut group = c.benchmark_group("direct_writes");
    let handle = Builder::new()
        .method(Method::Direct)
        .build()
        .expect("build direct handle");

    // Sector/page-aligned payloads. 4 KiB is the minimum required
    // for `Method::Direct` on every supported target. 16 MiB is the
    // upper bound where the bench remains tractable in CI under
    // Criterion's default sample budget.
    let sizes: &[(usize, &str)] = &[
        (4 * 1024, "4KiB"),
        (64 * 1024, "64KiB"),
        (1024 * 1024, "1MiB"),
        (16 * 1024 * 1024, "16MiB"),
    ];

    for &(size, sz_tag) in sizes {
        let data = vec![0xA5u8; size];
        let path = tmp_path(&format!("write_{sz_tag}"));
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(sz_tag),
            &path,
            |b, p: &PathBuf| {
                b.iter(|| handle.write(p, &data).expect("direct write"));
            },
        );
        let _ = std::fs::remove_file(&path);
    }
    group.finish();
}

fn bench_direct_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("direct_reads");
    let handle = Builder::new()
        .method(Method::Direct)
        .build()
        .expect("build direct handle");

    let sizes: &[(usize, &str)] = &[
        (4 * 1024, "4KiB"),
        (64 * 1024, "64KiB"),
        (1024 * 1024, "1MiB"),
        (16 * 1024 * 1024, "16MiB"),
    ];

    for &(size, sz_tag) in sizes {
        let data = vec![0x5Au8; size];
        let path = tmp_path(&format!("read_{sz_tag}"));
        handle.write(&path, &data).expect("priming write");
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(sz_tag),
            &path,
            |b, p: &PathBuf| {
                b.iter(|| {
                    let _ = handle.read(p).expect("direct read");
                });
            },
        );
        let _ = std::fs::remove_file(&path);
    }
    group.finish();
}

fn bench_direct_vs_sync_floor(c: &mut Criterion) {
    let mut group = c.benchmark_group("direct_vs_sync_floor");

    let direct = Builder::new()
        .method(Method::Direct)
        .build()
        .expect("direct handle");
    let sync = Builder::new()
        .method(Method::Sync)
        .build()
        .expect("sync handle");

    let sizes: &[(usize, &str)] = &[
        (4 * 1024, "4KiB"),
        (64 * 1024, "64KiB"),
        (1024 * 1024, "1MiB"),
    ];

    for &(size, sz_tag) in sizes {
        let data = vec![0xC3u8; size];
        let direct_path = tmp_path(&format!("vs_direct_{sz_tag}"));
        let sync_path = tmp_path(&format!("vs_sync_{sz_tag}"));
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("direct", sz_tag),
            &direct_path,
            |b, p: &PathBuf| b.iter(|| direct.write(p, &data).expect("direct write")),
        );
        group.bench_with_input(
            BenchmarkId::new("sync", sz_tag),
            &sync_path,
            |b, p: &PathBuf| b.iter(|| sync.write(p, &data).expect("sync write")),
        );
        let _ = std::fs::remove_file(&direct_path);
        let _ = std::fs::remove_file(&sync_path);
    }
    group.finish();
}

criterion_group!(
    direct_iouring,
    bench_direct_writes,
    bench_direct_reads,
    bench_direct_vs_sync_floor
);
criterion_main!(direct_iouring);
