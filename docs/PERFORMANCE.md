<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  PERFORMANCE
</h1>

fsys is designed for predictable latency on storage-engine
workloads. Numbers below are floor targets — values lower than
this are regressions blocking merge.

## Targets (commit-blocking)

| Operation | Target |
|---|---|
| `Method::Sync` 1 KiB write — consumer NVMe | < 2 ms p50 |
| `Method::Sync` 1 KiB write — enterprise NVMe (PLP) | < 500 µs p50 |
| `Method::Data` 1 KiB write — Linux NVMe | < 1 ms p50 |
| `Method::Direct` 1 KiB write — Linux NVMe + NVMe passthrough | **< 50 µs p50** |
| `Method::Direct` 1 KiB write — Linux NVMe (io_uring + fdatasync) | < 100 µs p50 |
| `Method::Direct` 1 KiB write — Windows NVMe + IOCTL | < 200 µs p50 |
| `JournalHandle::append` — single-threaded, no sync | **> 1 M ops/s** (bare Linux + NVMe; 200–500 K on Windows NTFS) |
| `JournalHandle::append_batch` (8 records, 4 KiB each) — single submit | ≥ 1.6× per-record reduction vs `append`-in-loop (0.9.1) |
| `JournalHandle::sync_through` (group-commit, 8 followers) | ≤ 1× the cost of one solo fsync (one syscall covers all followers) |
| Async overhead vs sync (`spawn_blocking` substrate) | < 50 µs added |
| Async overhead vs sync (native io_uring substrate, Linux + Direct, 0.7.0+) | within 5% of sync |
| Async batch overhead (oneshot vs crossbeam) | < 5 µs added |
| Native vs `spawn_blocking` substrate ratio (Linux + Direct, 4 KiB) | ≥ 1.1×; measured 1.46× on WSL2 + ext4 (D-8) |
| `write_copy` 1 KiB | within 10% of `write` |
| `Handle::copy` reflink (APFS / ReFS, 1 GiB file) | < 10 ms (vs seconds for fallback copy) |
| `scan` recursive — 10K files | < 50 ms |
| `find` `**/*.txt` — 10K files | < 100 ms |
| `count` recursive — 100K files | < 500 ms |

## Soak test discipline

Per locked decision D-7:

| Tier | Duration | Run when |
|---|---|---|
| Dev iteration | 60 s soak / 100K fuzz iterations | Per-checkpoint, before commit |
| CI nightly | 1 hour soak / 1M fuzz iterations | Nightly job on `main` |
| Release prep | 1 hour soak / 1M fuzz iterations | Before tagging alpha/beta/RC/1.0 |

Full-duration commands:

```sh
# Linux / macOS
cargo test --features stress --test stress -- --ignored --nocapture
cargo fuzz run path_normalize -- -max_total_time=3600
cargo fuzz run glob_pattern -- -max_total_time=3600
cargo fuzz run batch_builder -- -max_total_time=3600
```

```powershell
# Windows
cargo test --features stress --test stress -- --ignored --nocapture
cargo fuzz run path_normalize -- -max_total_time=3600
cargo fuzz run glob_pattern -- -max_total_time=3600
cargo fuzz run batch_builder -- -max_total_time=3600
```

Soak success criteria:
- < 5% RSS growth over 1 hour.
- 0 file descriptor leaks.
- 0 thread leaks.
- p99 latency within 2× of p50 at hour-end.

## Hardware sensitivity

- **NVMe vs SATA SSD vs HDD** — `Method::Direct` p50 latency
  scales with the device's flush latency. Consumer NVMe: ~50 µs;
  SATA SSD: ~500 µs; HDD: ~5–10 ms. The library's choice for
  `Method::Auto` reflects this — HDDs default to `Sync` because
  Direct's overhead dominates the device cost.
- **PLP (Power Loss Protection)** — when present, the controller
  acknowledges flush as soon as the data hits its volatile cache
  (the capacitor backs the cache through power loss). `Direct` p50
  on PLP-equipped NVMe is dominated by syscall overhead, not flush
  time.
- **Filesystem** — `O_DIRECT` is rejected on tmpfs, FUSE, and some
  network filesystems. fsys's runtime fallback transparently
  downgrades to `Data` (Linux) or `Sync` and updates
  `active_method()` so callers can observe the downgrade.

## Tuning knobs

`Builder` exposes:

- `io_uring_queue_depth(u32)` — Linux io_uring SQ depth. Default
  128. Higher depths help when the workload has many in-flight
  ops; lower depths reduce kernel memory.
- `buffer_pool_count(usize)` — number of aligned buffers in the
  per-handle pool. Default 64.
- `buffer_pool_block_size(usize)` — size of each buffer (in bytes,
  rounded up to the probed sector size). Default 4096.
- `batch_window_ms(u64)`, `batch_size_max(usize)`,
  `batch_queue_max(usize)` — group-lane dispatcher knobs from 0.4.0.
- `dispatcher_shards(usize)` (0.9.3) — number of dispatcher threads
  per handle. Default 1 (preserves pre-0.9.3 behavior exactly).
  Values > 1 spawn N independent dispatcher threads; batches are
  hash-routed by first op's path so within-batch order is
  preserved. Clamped to 1..=64. Raise on multi-core hosts where a
  single handle is the throughput bottleneck for parallel batch
  submitters writing to distinct files.
- `sqpoll(u32)` (0.9.7) — opt-in `IORING_SETUP_SQPOLL` with the
  given idle timeout in milliseconds. Spawns a kernel-side polling
  thread that drains the SQ without `io_uring_enter` syscalls.
  Linux-only consumption. Right call for sustained-throughput
  writers (database WAL flush loops, LSM compaction); wrong call
  for idle / low-rate workloads (kernel thread burns a CPU
  spinning).

### `Builder::tune_for(Workload)` presets (0.9.2)

For coordinated multi-knob tuning, prefer the workload preset
over hand-setting individual knobs:

| Workload | `buffer_pool_count` | `buffer_pool_block_size` | `io_uring_queue_depth` | `batch_queue_max` |
|---|---:|---:|---:|---:|
| `Workload::Default` | 64 | 4 KiB | 128 | 1024 |
| `Workload::Database` | 1024 | 8 KiB | 256 | 4096 |

`Workload::Database` is tuned for storage-engine workloads
(HiveDB, embedded KV stores, LSM trees) on NVMe with sustained
bulk writes. The 8 MiB pool footprint, 2× ring depth, and 4×
batch queue all coordinate to keep the dispatcher fed without
needing per-knob tweaks.

Apply presets **first**, then override individual knobs:

```rust
let fs = builder()
    .tune_for(Workload::Database)
    .observer(my_observer.clone())  // additive, no conflict
    .sqpoll(1000)                   // 0.9.7 — sustained-throughput opt-in
    .build()?;
```

## 0.9.x feature tuning guidance

| Capability | When to enable | When to leave default |
|---|---|---|
| `tune_for(Workload::Database)` | Storage-engine / KV / LSM workloads with sustained NVMe writes | Single-file or low-rate workloads |
| `dispatcher_shards(num_cpus::get())` | Concurrent batch submitters writing to distinct files | Single-writer workloads, latency-sensitive workloads |
| `Batch::commit_grouped()` | Bulk-load / SST-flush / checkpoint where the batch is the durability unit | Best-effort batches where per-op error visibility matters |
| `JournalHandle::append_batch` | N-record bulk inserts (database transactions, ledger batches) | Single-record append patterns |
| `JournalOptions::direct(true)` | Sustained sequential WAL workloads on NVMe with PLP | Mixed read/write, low-rate journals |
| `JournalOptions::sync_mode(SyncMode::Barrier)` | macOS Apple Silicon with PLP NVMe, journal workloads where eventual `Full` sync discipline is in place | macOS without PLP, or where every `sync_through` must be `F_FULLFSYNC` durability |
| `JournalOptions::write_lifetime_hint(Long)` | Linux multi-stream NVMe (drives that honour `F_SET_RW_HINT`) | Other drive types — no-op |
| `Builder::sqpoll(idle_ms)` | Sustained throughput writers on kernel ≥ 5.13 with `CAP_SYS_NICE` | Idle / low-rate workloads, sandboxed containers |
| `Builder::observer(...)` | Production observability (Prometheus, OpenTelemetry, custom metrics) | Tests / examples / quick-prototype code |

## Benchmarking

All benchmarks live in `benches/` and use Criterion. Run a
specific bench:

```sh
cargo bench --bench method_payload_matrix
cargo bench --bench mmap_workloads
cargo bench --bench direct_iouring
cargo bench --bench batch_throughput
```

See `benches/baselines.json` (added in 0.7.0 — F-9) for the
canonical performance baseline used by CI to detect regressions.
