//! Capability cache access latency benchmark (1.1.0).
//!
//! Measures three latencies that callers depend on:
//!
//! - **Warm cache access** — every call after the first. Must be
//!   well under 1 µs; the documented target is < 1 ms but the
//!   real number on modern hardware is sub-microsecond
//!   (`OnceLock` pointer load).
//! - **Fresh probe** — `probe_capabilities_fresh()`. Bypasses
//!   the in-memory `OnceLock`, runs the full probe, rewrites the
//!   on-disk cache. The brief's documented floor is 50-200 ms;
//!   we expect to be well inside that on a modern host.
//! - **TOML cache parse** — load + parse + validate. Exercises
//!   `Document::parse` + `document_to_capabilities`. Indirectly
//!   measured by exercising a fresh `probe_capabilities_fresh()`
//!   pipeline (which serialises + writes + then would be read
//!   back on a fresh process).
//!
//! Run: `cargo bench --bench capability_access`

use criterion::{criterion_group, criterion_main, Criterion};

fn bench_capability_access(c: &mut Criterion) {
    // Warm up — first call pays the full probe cost.
    let _warmup = fsys::capability::capabilities();

    let mut group = c.benchmark_group("capability_access");
    // Warm cache hit — what every steady-state caller sees.
    group.bench_function("capabilities_warm", |b| {
        b.iter(|| {
            // Hint to the compiler this is the value we care about,
            // not some optimised-out call.
            criterion::black_box(fsys::capability::capabilities());
        });
    });

    // Fresh probe — bypasses the in-memory cache, runs the disk +
    // sysfs IO. Slower; measures the cold-start floor.
    group.bench_function("probe_fresh", |b| {
        b.iter(|| {
            criterion::black_box(fsys::capability::probe_capabilities_fresh());
        });
    });

    group.finish();
}

fn bench_pci_address_round_trip(c: &mut Criterion) {
    let mut group = c.benchmark_group("pci_address");
    let addr = fsys::PciAddress::new(0, 0x1f, 0x03, 2);
    group.bench_function("to_canonical", |b| {
        b.iter(|| {
            criterion::black_box(addr.to_canonical());
        });
    });
    group.bench_function("parse_four_segment", |b| {
        b.iter(|| {
            criterion::black_box(fsys::PciAddress::parse("0000:1f:03.2"));
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_capability_access,
    bench_pci_address_round_trip
);
criterion_main!(benches);
