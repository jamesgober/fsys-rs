//! Native io_uring async substrate vs `spawn_blocking` fallback —
//! head-to-head Criterion bench.
//!
//! Per locked decision D-8 in `.dev/DECISIONS-0.7.0.md`, the
//! load-bearing performance metric for 0.7.0 is the **2× advantage
//! of native io_uring async over `spawn_blocking`** on Linux
//! Direct async writes. This bench is the regression detector for
//! that property.
//!
//! ## Methodology
//!
//! Two Criterion benchmark groups, both writing the same 4 KiB
//! payload to a temp file via `Method::Direct`:
//!
//! 1. **`async_blocking_write_4k`** — `FSYS_DISABLE_NATIVE_ASYNC=1`
//!    forces the substrate to `SpawnBlocking`. Every async op
//!    bounces through `tokio::task::spawn_blocking`.
//! 2. **`async_native_write_4k`** — env override unset; substrate
//!    transitions to `NativeIoUring` after the first warmup op.
//!    Submissions go directly through io_uring.
//!
//! Both run on the same machine; the relative speedup is what we
//! measure. Per D-8, WSL is partially valid for this comparison
//! because both paths share the same virtualisation tax — the
//! ratio is preserved (within ±20%) compared to bare metal.
//!
//! ## Interpretation thresholds
//!
//! - **WSL ≥ 1.5×** → suggestive that bare metal hits the 2×
//!   target; tier-3 release-prep cert proceeds.
//! - **WSL ~ 1.1×** → something is wrong with the substrate;
//!   investigate.
//! - **WSL < 1.0×** (native slower) → hard failure; substrate is
//!   broken.
//!
//! The bench compiles and runs on every platform; on non-Linux
//! the native path is unreachable and only `spawn_blocking` is
//! exercised. The Linux-specific `async_native_write_4k` group
//! is conditionally compiled.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fsys::{builder, Method};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::runtime::Runtime;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_bench_native_vs_blocking_{}_{}_{}.dat",
        std::process::id(),
        n,
        tag
    ))
}

fn build_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn bench_async_blocking_write_4k(c: &mut Criterion) {
    let rt = build_runtime();
    // Force the SpawnBlocking substrate via env override so the
    // bench measures the fallback path even on Linux + Direct.
    // SAFETY: single-threaded bench process; no concurrent env
    // mutation. Restored at the end of the function.
    let prior = std::env::var_os("FSYS_DISABLE_NATIVE_ASYNC");
    unsafe {
        std::env::set_var("FSYS_DISABLE_NATIVE_ASYNC", "1");
    }

    let fs = Arc::new(builder().method(Method::Direct).build().expect("handle"));
    let payload = vec![0xA5u8; 4096];

    let mut group = c.benchmark_group("async_blocking_write_4k");
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function("write_async_blocking_path", |b| {
        b.iter(|| {
            let path = tmp_path("blocking");
            let fs = fs.clone();
            let payload = payload.clone();
            rt.block_on(async move {
                let _ = fs.write_async(&path, payload).await;
                let _ = std::fs::remove_file(&path);
            });
        });
    });
    group.finish();

    // Restore env. SAFETY: same as set above.
    unsafe {
        match prior {
            Some(v) => std::env::set_var("FSYS_DISABLE_NATIVE_ASYNC", v),
            None => std::env::remove_var("FSYS_DISABLE_NATIVE_ASYNC"),
        }
    }
}

#[cfg(target_os = "linux")]
fn bench_async_native_write_4k(c: &mut Criterion) {
    let rt = build_runtime();
    let fs = Arc::new(builder().method(Method::Direct).build().expect("handle"));

    // Warmup: trigger native-substrate construction before the
    // measured iters begin. The first async Direct op
    // constructs the async ring + driver task; subsequent ops
    // hit the cached substrate.
    {
        let fs = fs.clone();
        let warmup_path = tmp_path("native_warmup");
        rt.block_on(async move {
            let _ = fs.write_async(&warmup_path, vec![0u8; 4096]).await;
            let _ = std::fs::remove_file(&warmup_path);
        });
    }

    let payload = vec![0xC3u8; 4096];

    let mut group = c.benchmark_group("async_native_write_4k");
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function("write_async_native_path", |b| {
        b.iter(|| {
            let path = tmp_path("native");
            let fs = fs.clone();
            let payload = payload.clone();
            rt.block_on(async move {
                let _ = fs.write_async(&path, payload).await;
                let _ = std::fs::remove_file(&path);
            });
        });
    });
    group.finish();
}

#[cfg(not(target_os = "linux"))]
fn bench_async_native_write_4k(_c: &mut Criterion) {
    // Native substrate is Linux-only (D-1). On other platforms
    // there's no native path to bench; skip silently. The
    // spawn_blocking bench above carries the cross-platform
    // throughput baseline.
}

criterion_group!(
    benches,
    bench_async_blocking_write_4k,
    bench_async_native_write_4k
);
criterion_main!(benches);
