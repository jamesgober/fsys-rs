//! # `Builder::sqpoll(idle_ms)` — kernel-side io_uring submission polling
//!
//! 0.9.7 added opt-in `IORING_SETUP_SQPOLL` for the per-handle io_uring
//! sync ring. With SQPOLL enabled, the kernel spawns (or shares) a
//! polling thread that drains the submission queue without requiring
//! `io_uring_enter` syscalls. After `idle_ms` of no submissions the
//! kernel thread sleeps and the next push wakes it via a one-time
//! `io_uring_enter` syscall.
//!
//! ## When to use this pattern
//!
//! - **Sustained-throughput writers** — database WAL flush loops,
//!   LSM-tree compaction, anywhere the io_uring submission rate is
//!   high enough that the per-`io_uring_enter` syscall overhead
//!   matters. SQPOLL eliminates that syscall in the steady state.
//! - **Kernel ≥ 5.13** with `CAP_SYS_NICE` (or root) on older
//!   kernels. The capability requirement was relaxed in 5.13.
//!
//! ## When NOT to use this pattern
//!
//! - **Idle / low-rate workloads.** The kernel polling thread spins
//!   on a CPU when active; for a low-rate workload that thread sits
//!   idle most of the time, wasting cycles.
//! - **Containers / sandboxes** without `CAP_SYS_NICE` on older
//!   kernels. `io_uring_setup` returns `EPERM` and fsys falls back
//!   to non-SQPOLL `pwrite + fdatasync` — same durability, slower
//!   path. The fallback is silent + observable via
//!   `Handle::active_durability_primitive`.
//!
//! ## Tuning `idle_ms`
//!
//! Typical values:
//!
//! - `1000`–`5000` (1–5 s) for steady-state workloads with
//!   predictable load
//! - `100`–`200` for bursty workloads where you want fast wake +
//!   moderate idle cost
//!
//! Lower `idle_ms` keeps the kernel thread spinning more
//! aggressively; higher values let it sleep more.
//!
//! ## Platform availability
//!
//! Linux-only consumption. macOS and Windows ignore the value
//! (the io_uring sync ring doesn't exist on those platforms by
//! design).
//!
//! Run: `cargo run --example 25_sqpoll_opt_in`

use std::sync::Arc;

fn main() -> fsys::Result<()> {
    // Opt the per-handle io_uring sync ring into SQPOLL with a 1-second
    // idle timeout. On Linux + sufficient privilege this engages the
    // kernel polling thread; elsewhere it's a transparent no-op.
    let fs = Arc::new(
        fsys::builder()
            .method(fsys::Method::Direct)
            .sqpoll(1000)
            .build()?,
    );

    println!("handle built with sqpoll(1000)");
    println!("  active method:    {:?}", fs.active_method());
    println!("  active primitive: {:?}", fs.active_durability_primitive());

    // Exercise the Direct path — on a SQPOLL-capable Linux host this
    // is where the syscall savings appear. On Windows / macOS the
    // sqpoll(...) call was a no-op; the same Direct path fires.
    let path = std::env::temp_dir().join("fsys_example_sqpoll.dat");
    let _ = std::fs::remove_file(&path);

    let payload = vec![0xABu8; 64 * 1024]; // 64 KiB aligned
    let start = std::time::Instant::now();
    for _ in 0..100 {
        fs.write(&path, &payload)?;
    }
    let elapsed = start.elapsed();
    println!(
        "100 atomic-replace writes (64 KiB each) in {elapsed:?} ({:.0} ops/sec)",
        100.0 / elapsed.as_secs_f64()
    );

    println!();
    println!("note: SQPOLL only fires on Linux. Windows / macOS callers");
    println!("are exercising the same path as without sqpoll(...) — the");
    println!("knob is captured but never consulted on those platforms.");

    let _ = std::fs::remove_file(&path);
    Ok(())
}
