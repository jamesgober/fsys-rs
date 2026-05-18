//! Journal backend observability accessor benchmark (1.1.0).
//!
//! The brief mandates that
//! [`JournalHandle::backend_kind`](fsys::JournalHandle::backend_kind),
//! [`backend_health`](fsys::JournalHandle::backend_health), and
//! [`backend_info`](fsys::JournalHandle::backend_info) are safe to
//! call from per-second health-check loops. This benchmark measures
//! the actual cost per call so any future regression is caught.
//!
//! Floor expectations on modern hardware:
//!
//! - `backend_kind` — well under 50 ns. Plain field read + cfg-gated
//!   `OnceLock` peek on Linux + async; a single branch otherwise.
//! - `backend_health` — same floor; today the kernel path returns
//!   an `empty()` snapshot, future versions will surface live
//!   counters via atomic reads.
//! - `backend_info` — slightly more; allocates a `String` for the
//!   `selection_reason` field and captures the call-time
//!   `SystemTime`. Target: < 1 µs.
//!
//! Run: `cargo bench --bench backend_accessors`

use criterion::{criterion_group, criterion_main, Criterion};
use std::sync::Arc;

fn open_test_journal() -> (Arc<fsys::JournalHandle>, std::path::PathBuf) {
    let fs = fsys::builder().build().expect("build handle");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("fsys_bench_backend_{nanos}.wal"));
    let _ = std::fs::remove_file(&path);
    let log = fs.journal(&path).expect("open journal");
    (Arc::new(log), path)
}

fn bench_backend_accessors(c: &mut Criterion) {
    let (log, path) = open_test_journal();

    let mut group = c.benchmark_group("backend_accessors");

    group.bench_function("backend_kind", |b| {
        b.iter(|| {
            criterion::black_box(log.backend_kind());
        });
    });

    group.bench_function("backend_health", |b| {
        b.iter(|| {
            criterion::black_box(log.backend_health());
        });
    });

    group.bench_function("backend_info", |b| {
        b.iter(|| {
            criterion::black_box(log.backend_info());
        });
    });

    group.finish();

    drop(log);
    let _ = std::fs::remove_file(&path);
}

criterion_group!(benches, bench_backend_accessors);
criterion_main!(benches);
