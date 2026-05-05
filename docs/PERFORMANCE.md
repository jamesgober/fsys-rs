# Performance

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
| Async overhead vs sync (`spawn_blocking`) | < 50 µs added |
| Async batch overhead (oneshot vs crossbeam) | < 5 µs added |
| `write_copy` 1 KiB | within 10% of `write` |
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
- `buffer_pool_size(usize)` — number of aligned buffers in the
  per-handle pool. Default 64.
- `buffer_pool_block(usize)` — size of each buffer (in bytes,
  rounded up to the probed sector size). Default 4096.
- `batch_window_ms(u64)`, `batch_size_max(usize)`,
  `batch_queue_max(usize)` — group-lane dispatcher knobs from 0.4.0.

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
