//! Mmap-specific workload benchmarks.
//!
//! Validates two claims from `.dev/DECISIONS-0.5.0.md`:
//!
//! 1. **R-2'' (suitability fallback):** sub-page payloads transparently
//!    fall back to `Method::Sync`. The bench labels the run with the
//!    fallback so wall-clock comparisons against the page-aligned
//!    cell are interpretable rather than misleading.
//! 2. **Mmap is competitive on large page-aligned writes.** For
//!    payloads ≥ 1 MiB the mapping + msync path should beat or match
//!    `Sync` on platforms where the kernel can elide redundant
//!    page-cache copies — this bench is the regression detector for
//!    that property.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_bench_mmap_{}_{}_{}.dat",
        std::process::id(),
        n,
        tag
    ))
}

fn page_size() -> usize {
    fsys::os::info().page_size.max(4096)
}

fn bench_mmap_aligned_writes(c: &mut Criterion) {
    let page = page_size();
    let mut group = c.benchmark_group("mmap_page_aligned_writes");
    let handle = Builder::new()
        .method(Method::Mmap)
        .build()
        .expect("build mmap handle");

    // Multiples of the host page size, scaling up to where mmap is
    // expected to dominate kernel-copy paths.
    let sizes: &[(usize, &str)] = &[
        (page, "1page"),
        (16 * page, "16page"),
        (256 * page, "256page"),
        (4096 * page, "4096page"),
    ];

    for &(size, sz_tag) in sizes {
        let data = vec![0xA5u8; size];
        let path = tmp_path(&format!("aligned_{sz_tag}"));
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(sz_tag),
            &path,
            |b, p: &PathBuf| {
                b.iter(|| handle.write(p, &data).expect("write"));
            },
        );
        let _ = std::fs::remove_file(&path);
    }
    group.finish();
}

fn bench_mmap_subpage_fallback(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap_subpage_sync_fallback");
    let handle = Builder::new()
        .method(Method::Mmap)
        .build()
        .expect("build mmap handle");

    // Sub-page payloads — `Method::Mmap` will permanently fall back
    // to `Method::Sync` for these (R-2''). The bench cell names mark
    // the fallback explicitly.
    let sizes: &[(usize, &str)] = &[
        (16, "16B"),
        (256, "256B"),
        (1024, "1KiB"),
        (3 * 1024, "3KiB"),
    ];

    for &(size, sz_tag) in sizes {
        let data = vec![0x5Au8; size];
        let path = tmp_path(&format!("subpage_{sz_tag}"));
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("fallback_{sz_tag}")),
            &path,
            |b, p: &PathBuf| {
                b.iter(|| handle.write(p, &data).expect("write"));
            },
        );
        let _ = std::fs::remove_file(&path);
    }
    group.finish();
}

fn bench_mmap_vs_sync_at_size(c: &mut Criterion) {
    let page = page_size();
    let mut group = c.benchmark_group("mmap_vs_sync_aligned");

    let mmap = Builder::new()
        .method(Method::Mmap)
        .build()
        .expect("mmap handle");
    let sync = Builder::new()
        .method(Method::Sync)
        .build()
        .expect("sync handle");

    let sizes: &[(usize, &str)] = &[
        (16 * page, "16page"),
        (256 * page, "256page"),
        (4096 * page, "4096page"),
    ];

    for &(size, sz_tag) in sizes {
        let data = vec![0xC3u8; size];
        let mmap_path = tmp_path(&format!("vs_mmap_{sz_tag}"));
        let sync_path = tmp_path(&format!("vs_sync_{sz_tag}"));
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("mmap", sz_tag),
            &mmap_path,
            |b, p: &PathBuf| b.iter(|| mmap.write(p, &data).expect("mmap write")),
        );
        group.bench_with_input(
            BenchmarkId::new("sync", sz_tag),
            &sync_path,
            |b, p: &PathBuf| b.iter(|| sync.write(p, &data).expect("sync write")),
        );
        let _ = std::fs::remove_file(&mmap_path);
        let _ = std::fs::remove_file(&sync_path);
    }
    group.finish();
}

criterion_group!(
    mmap_workloads,
    bench_mmap_aligned_writes,
    bench_mmap_subpage_fallback,
    bench_mmap_vs_sync_at_size
);
criterion_main!(mmap_workloads);
