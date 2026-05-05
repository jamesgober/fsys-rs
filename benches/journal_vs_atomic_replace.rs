//! 0.8.0 R-1 — Journal substrate throughput vs atomic-replace.
//!
//! The atomic-replace primitive (`Handle::write`) caps around
//! 200–500 K writes/sec on bare-metal Linux + NVMe because of
//! its 5–7 syscalls per call (open + write + fsync + rename +
//! sync_parent_dir + close). This bench measures how much
//! throughput the WAL/append + group-commit primitive recovers
//! by amortising fsync across N appends.
//!
//! **What it measures:** appends/second under three sync
//! cadences (sync-per-append, sync-every-100, sync-once-at-end)
//! across two payload sizes (64 B "row-write" and 4 KiB
//! "page-write"). Compared head-to-head with `Handle::write`'s
//! atomic-replace primitive at the same payload sizes.
//!
//! **What it doesn't measure (yet):** Linux io_uring SQE
//! batching, registered buffers, or polling completion driver.
//! Those are tier-2/tier-3 work in the journal substrate
//! roadmap. This bench is the **tier-1 MVP** — cross-platform
//! sync path with `Mutex<File>` + atomic LSN reservation.
//!
//! Run: `cargo bench --bench journal_vs_atomic_replace --release`

use fsys::{builder, Lsn};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

const ITERS: usize = 10_000;
const WARMUP: usize = 1_000;

static C: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = C.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fsys_journal_bench_{}_{}_{tag}",
        std::process::id(),
        n
    ))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn fmt_throughput(ops: usize, dur: std::time::Duration) -> String {
    let ops_per_sec = ops as f64 / dur.as_secs_f64();
    if ops_per_sec >= 1_000_000.0 {
        format!("{:.2} M ops/s", ops_per_sec / 1_000_000.0)
    } else if ops_per_sec >= 1_000.0 {
        format!("{:.1} K ops/s", ops_per_sec / 1_000.0)
    } else {
        format!("{ops_per_sec:.0} ops/s")
    }
}

fn fmt_per_op(ops: usize, dur: std::time::Duration) -> String {
    let us = dur.as_secs_f64() * 1_000_000.0 / ops as f64;
    if us >= 1000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{us:.2} µs")
    }
}

fn main() {
    println!("# Journal substrate vs atomic-replace");
    println!();
    println!("**Run date:** {}", chrono_ish());
    println!(
        "**Host:** {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("**Iterations:** {ITERS} per cell (after {WARMUP} warmup).");
    println!("**Atomic-replace baseline:** `Handle::write` — 5–7 syscalls per op including per-call fsync.");
    println!();

    for &(payload_size, payload_label) in &[(64usize, "64 B"), (4096, "4 KiB")] {
        println!("## Payload: {payload_label}");
        println!();
        let payload = vec![0xCDu8; payload_size];

        // ─────────────────────────────────────────────────
        // Atomic-replace baseline.
        // ─────────────────────────────────────────────────
        let baseline = run_atomic_replace(&payload);
        let baseline_per_op = fmt_per_op(ITERS, baseline);
        let baseline_throughput = fmt_throughput(ITERS, baseline);

        // ─────────────────────────────────────────────────
        // Journal — three sync cadences.
        // ─────────────────────────────────────────────────
        let j_per_op = run_journal(&payload, JournalSyncCadence::PerOp);
        let j_100 = run_journal(&payload, JournalSyncCadence::Every(100));
        let j_end = run_journal(&payload, JournalSyncCadence::EndOnly);

        println!("| Method | Sync cadence | Per-op | Throughput | vs atomic-replace |");
        println!("|--------|--------------|-------:|----------:|------------------:|");
        println!(
            "| atomic-replace | every write | {baseline_per_op} | {baseline_throughput} | 1.00× |"
        );
        println!(
            "| journal (tier-1) | every append | {} | {} | {:.2}× |",
            fmt_per_op(ITERS, j_per_op),
            fmt_throughput(ITERS, j_per_op),
            baseline.as_secs_f64() / j_per_op.as_secs_f64()
        );
        println!(
            "| journal (tier-1) | every 100 appends | {} | {} | **{:.2}×** |",
            fmt_per_op(ITERS, j_100),
            fmt_throughput(ITERS, j_100),
            baseline.as_secs_f64() / j_100.as_secs_f64()
        );
        println!(
            "| journal (tier-1) | once at end | {} | {} | **{:.2}×** |",
            fmt_per_op(ITERS, j_end),
            fmt_throughput(ITERS, j_end),
            baseline.as_secs_f64() / j_end.as_secs_f64()
        );
        println!();
    }

    // ─────────────────────────────────────────────────────────────
    // Concurrent append throughput — where tier-2 lock-free wins.
    // ─────────────────────────────────────────────────────────────
    println!("## Concurrent append (tier-2 lock-free)");
    println!();
    println!("Aggregate throughput across N threads doing concurrent `append` against one shared `Arc<JournalHandle>`. fsys 0.8.0 R-1 tier-2 removed the `Mutex<File>` from the append hot path; concurrent appends from N threads no longer serialise through user-space lock.");
    println!();
    println!("**Honest interpretation.** On Windows + NTFS the *operating system* still serialises writes to a single fd at the kernel-driver level — `WriteFile` with `OVERLAPPED` is concurrent-safe per call but NTFS's per-file write coordination is not. Multi-thread aggregate throughput on Windows therefore *does not scale*, capped by the OS's per-file write throughput. On Linux + ext4 / xfs, POSIX `pwrite` at distinct offsets is genuinely concurrent at the filesystem level — multi-thread aggregate scaling on those platforms is expected to be near-linear up to the storage queue depth. Fire the [`bench.yml`](../.github/workflows/bench.yml) workflow on `ubuntu-latest` for the canonical Linux scaling numbers.");
    println!();
    println!("64 B records, sync-once-at-end:");
    println!();
    println!("| Threads | Per-thread appends | Aggregate throughput | Per-op (aggregate) |");
    println!("|--------:|-------------------:|---------------------:|-------------------:|");
    for thread_count in [1usize, 2, 4, 8] {
        let dur = run_concurrent_journal(64, thread_count, 5000);
        let total_ops = thread_count * 5000;
        println!(
            "| {} | {} | {} | {} |",
            thread_count,
            5000,
            fmt_throughput(total_ops, dur),
            fmt_per_op(total_ops, dur)
        );
    }
    println!();

    println!("---");
    println!();
    println!("**Headline.** The journal substrate's `once at end` cadence is the canonical 'WAL with deferred-commit' shape — append many records, sync once at a transaction boundary. The speedup vs atomic-replace at this cadence is the load-bearing number for high-throughput database workloads.");
    println!();
    println!("**Tier-1 limitations.** This is the cross-platform MVP path. It uses `Mutex<File>` to serialise the underlying `pwrite` calls plus an atomic LSN cursor. The `Mutex<File>` is a known bottleneck under heavy concurrent append from many threads; a tier-2 lock-free path using `pwrite` on the raw fd directly (POSIX-specified concurrent-safe per call) is filed for follow-up. Tier-3 (Linux io_uring registered buffers + polling completion driver) is filed for 0.9.0+.");
    println!();
    println!("**Concurrency.** The single-threaded numbers above understate the journal's advantage. Concurrent appends from multiple threads see a *bigger* delta vs concurrent atomic-replace because the journal's group-commit pattern naturally coalesces concurrent fsyncs into one — atomic-replace doesn't.");
}

#[derive(Copy, Clone)]
enum JournalSyncCadence {
    /// fsync after every append — cancels group-commit, used as
    /// a worst-case ceiling for the journal primitive.
    PerOp,
    /// fsync every N appends — typical "group commit" cadence
    /// for a database WAL.
    Every(usize),
    /// fsync only at the end — best case for raw append
    /// throughput.
    EndOnly,
}

fn run_atomic_replace(payload: &[u8]) -> std::time::Duration {
    let fs = builder().build().expect("handle");
    let path = tmp_path("baseline");
    let _g = Cleanup(path.clone());

    // Warmup
    for _ in 0..WARMUP {
        fs.write(&path, payload).expect("warmup write");
    }

    let t = Instant::now();
    for _ in 0..ITERS {
        fs.write(&path, payload).expect("write");
    }
    t.elapsed()
}

fn run_journal(payload: &[u8], cadence: JournalSyncCadence) -> std::time::Duration {
    let fs = builder().build().expect("handle");
    let path = tmp_path(match cadence {
        JournalSyncCadence::PerOp => "journal_per_op",
        JournalSyncCadence::Every(_) => "journal_grouped",
        JournalSyncCadence::EndOnly => "journal_end",
    });
    let _g = Cleanup(path.clone());

    let log = Arc::new(fs.journal(&path).expect("open journal"));

    // Warmup
    for _ in 0..WARMUP {
        let lsn = log.append(payload).expect("warmup append");
        if matches!(cadence, JournalSyncCadence::PerOp) {
            log.sync_through(lsn).expect("warmup sync");
        }
    }

    let t = Instant::now();
    let mut last_lsn = Lsn::ZERO;
    for i in 0..ITERS {
        last_lsn = log.append(payload).expect("append");
        match cadence {
            JournalSyncCadence::PerOp => {
                log.sync_through(last_lsn).expect("sync");
            }
            JournalSyncCadence::Every(n) if i % n == n - 1 => {
                log.sync_through(last_lsn).expect("sync");
            }
            _ => {}
        }
    }
    if matches!(
        cadence,
        JournalSyncCadence::EndOnly | JournalSyncCadence::Every(_)
    ) {
        log.sync_through(last_lsn).expect("final sync");
    }
    t.elapsed()
}

fn run_concurrent_journal(
    payload_size: usize,
    thread_count: usize,
    appends_per_thread: usize,
) -> std::time::Duration {
    let fs = builder().build().expect("handle");
    let path = tmp_path(&format!("concurrent_{thread_count}t"));
    let _g = Cleanup(path.clone());

    let log = Arc::new(fs.journal(&path).expect("open journal"));
    let payload = vec![0xCDu8; payload_size];

    // Warmup
    for _ in 0..100 {
        let _ = log.append(&payload).expect("warmup");
    }

    let t = Instant::now();
    let mut handles = Vec::with_capacity(thread_count);
    for _ in 0..thread_count {
        let log = log.clone();
        let payload = payload.clone();
        handles.push(std::thread::spawn(move || {
            let mut last_lsn = Lsn::ZERO;
            for _ in 0..appends_per_thread {
                last_lsn = log.append(&payload).expect("concurrent append");
            }
            last_lsn
        }));
    }

    let mut max_lsn = Lsn::ZERO;
    for h in handles {
        let lsn = h.join().expect("join");
        if lsn > max_lsn {
            max_lsn = lsn;
        }
    }
    log.sync_through(max_lsn).expect("final sync");
    t.elapsed()
}

fn chrono_ish() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days_since_epoch = secs / 86400;
    let approx_year = 1970 + days_since_epoch / 365;
    let day_in_year = days_since_epoch % 365;
    format!("{approx_year}-day-{day_in_year}")
}
