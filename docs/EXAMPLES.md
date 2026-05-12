<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  EXAMPLES
</h1>

The [`examples/`](../examples/) directory contains **29 runnable examples** covering every part of the public API. Each example is self-contained, comment-documented, and produces visible output so you can confirm the path you exercised. Examples 18–29 cover the 0.9.1–0.9.7 surface additions added in the 0.9.8 polish release.

## Running

Each file is a standalone `cargo` example. Run by name:

```sh
# Sync examples (any platform)
cargo run --example 01_quick_one_shot
cargo run --example 02_handle_basics
cargo run --example 03_method_sync
cargo run --example 04_method_data
cargo run --example 05_method_direct
cargo run --example 06_method_mmap
cargo run --example 07_method_auto
cargo run --example 08_write_copy
cargo run --example 09_batch_slice
cargo run --example 10_batch_builder
cargo run --example 13_root_scoped
cargo run --example 14_directory_crud
cargo run --example 15_error_handling
cargo run --example 16_tuning_direct
cargo run --example 17_journal_basics

# Async examples (require the `async` feature)
cargo run --example 11_async_basics --features async
cargo run --example 12_async_batch  --features async

# 0.9.1–0.9.7 capability examples (added in 0.9.8)
cargo run --example 18_journal_append_batch
cargo run --example 19_batch_commit_grouped
cargo run --example 20_tune_for_database
cargo run --example 21_punch_hole_wal_trim
cargo run --example 22_sync_mode_barrier_macos
cargo run --example 23_multi_shard_batches
cargo run --example 24_observer_basics
cargo run --example 25_sqpoll_opt_in
cargo run --example 26_plp_aware_skip_fsync
cargo run --example 27_atomic_write_unit
cargo run --example 28_write_lifetime_hint
cargo run --example 29_reflink_aware_copy
```

For release-mode timings (closer to production), append `--release`. The dev profile is fine for "does it work?" validation; benches in [`benches/`](../benches/) are the right tool for actual perf numbers.

## Catalogue

### Three-tier entry points

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **01** | [`01_quick_one_shot.rs`](../examples/01_quick_one_shot.rs) | `fsys::quick::write` / `fsys::quick::read` | One-off ops where building a `Handle` is overkill. |
| **02** | [`02_handle_basics.rs`](../examples/02_handle_basics.rs) | `builder().build()` + reusing one `Handle` for many ops | Any program that does more than a single IO op. |

### `Method` variants

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **03** | [`03_method_sync.rs`](../examples/03_method_sync.rs) | `Method::Sync` — `fsync(2)` family on every platform | Strongest durability; safest universal default. |
| **04** | [`04_method_data.rs`](../examples/04_method_data.rs) | `Method::Data` — `fdatasync` on Linux, falls back to `Sync` elsewhere | Linux + small in-place updates. |
| **05** | [`05_method_direct.rs`](../examples/05_method_direct.rs) | `Method::Direct` — bypass the OS page cache | Storage engines / databases that own their own cache. |
| **06** | [`06_method_mmap.rs`](../examples/06_method_mmap.rs) | `Method::Mmap` — memory-mapped IO with `msync` | Read-heavy random-access workloads. |
| **07** | [`07_method_auto.rs`](../examples/07_method_auto.rs) | `Method::Auto` — hardware-aware automatic selection | Default — let `fsys` pick. |

### Atomic-replace + metadata preservation

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **08** | [`08_write_copy.rs`](../examples/08_write_copy.rs) | `write_copy` — atomic-swap preserving target metadata | Replacing a config file at `/etc/foo.conf` whose mode/ACL/owner is set by an admin. |

### Batch APIs

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **09** | [`09_batch_slice.rs`](../examples/09_batch_slice.rs) | `write_batch` / `delete_batch` — slice submission | You already have a `Vec<(path, data)>`. |
| **10** | [`10_batch_builder.rs`](../examples/10_batch_builder.rs) | `Handle::batch().write(...).delete(...).commit()` — fluent builder | Large or dynamic batches built across a loop. |

### Async layer

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **11** | [`11_async_basics.rs`](../examples/11_async_basics.rs) | `write_async` / `read_async` + `Handle::async_substrate()` | Any tokio-based application; use `async_substrate()` to confirm the native io_uring path engaged. |
| **12** | [`12_async_batch.rs`](../examples/12_async_batch.rs) | `write_batch_async` / `delete_batch_async` | Async programs that need durable batch submission. |

### Sandboxing + directory ops

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **13** | [`13_root_scoped.rs`](../examples/13_root_scoped.rs) | `Builder::root(...)` — bind a handle to a base dir; verify `..` escape rejection | Any program that should not write outside a known subtree (HTTP upload handler, build tool, test fixture). |
| **14** | [`14_directory_crud.rs`](../examples/14_directory_crud.rs) | `mkdir_all` / `scan` vs `scan_all` / `find` / `count_all` | Walking a directory tree; demonstrates the flat-vs-recursive name split. |

### Error handling + tuning

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **15** | [`15_error_handling.rs`](../examples/15_error_handling.rs) | Match on `Error::code()` — stable `FS-NNNNN` codes for log-grep | Production error handling and structured logs. |
| **16** | [`16_tuning_direct.rs`](../examples/16_tuning_direct.rs) | `Builder::buffer_pool_count` / `buffer_pool_block_size` / `io_uring_queue_depth` | Workload-specific tuning of `Method::Direct`. |

### Journal substrate (high-throughput WAL)

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **17** | [`17_journal_basics.rs`](../examples/17_journal_basics.rs) | `Handle::journal` + `JournalHandle::append` + group-commit `sync_through`; head-to-head timing vs atomic-replace at the same workload | Database WAL, queue persistence, ledger append — anywhere you need millions of durable writes/sec rather than per-call atomic-replace. The example produces a 100×+ speedup demonstration on a typical machine. |
| **18** | [`18_journal_append_batch.rs`](../examples/18_journal_append_batch.rs) | `JournalHandle::append_batch(&[&[u8]])` — 64 records in one syscall (~1.6× per-record win vs `append`-in-loop, larger on Linux NVMe) | Multi-row transactions, batch ledger entries — any workload where the natural unit is "N records committed together." *(0.9.1)* |

### Bulk-load + grouped commit (0.9.3+)

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **19** | [`19_batch_commit_grouped.rs`](../examples/19_batch_commit_grouped.rs) | `Batch::commit_grouped()` — atomic-batch fsync with amortised parent-dir syncs (1 sync per unique parent dir, not 1 per op) | Bulk loads, SST flushes, database checkpoint emissions — workloads where the batch is the durability unit, not the individual op. *(0.9.3)* |
| **23** | [`23_multi_shard_batches.rs`](../examples/23_multi_shard_batches.rs) | `Builder::dispatcher_shards(4)` — N-shard batch dispatcher with hash-routing by first-op path | Multi-core hosts with parallel batch submitters writing to distinct files. *(0.9.3)* |

### Workload presets + tuning (0.9.2+)

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **20** | [`20_tune_for_database.rs`](../examples/20_tune_for_database.rs) | `Builder::tune_for(Workload::Database)` — coordinated 4-knob preset for storage-engine workloads + after-preset override pattern | Storage-engine / KV / LSM workloads with sustained NVMe writes. *(0.9.2)* |
| **24** | [`24_observer_basics.rs`](../examples/24_observer_basics.rs) | `FsysObserver` trait + `Builder::observer` — atomic-counter telemetry hook firing on every journal append/sync | Production observability (Prometheus, OpenTelemetry, custom metrics). *(0.9.2)* |
| **25** | [`25_sqpoll_opt_in.rs`](../examples/25_sqpoll_opt_in.rs) | `Builder::sqpoll(idle_ms)` — opt-in kernel-side io_uring SQ polling | Sustained-throughput Direct-IO writers on Linux ≥ 5.13. *(0.9.7)* |

### Hardware-aware patterns (0.9.2 / 0.9.4)

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **26** | [`26_plp_aware_skip_fsync.rs`](../examples/26_plp_aware_skip_fsync.rs) | `Handle::is_plp_protected()` / `plp_status()` — confirmed Power-Loss-Protection detection for safe per-commit fsync skip | OLTP transaction commits on enterprise NVMe — 3–10× throughput win when PLP is confirmed. *(0.9.2)* |
| **27** | [`27_atomic_write_unit.rs`](../examples/27_atomic_write_unit.rs) | `Handle::atomic_write_unit()` — NVMe NAWUN/NAWUPF probe for torn-write-free guarantee detection | Database engines wanting to skip per-page CRC when NAWUN ≥ page size. *(0.9.4, Linux)* |

### Cross-platform sync tuning (0.9.4)

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **22** | [`22_sync_mode_barrier_macos.rs`](../examples/22_sync_mode_barrier_macos.rs) | `JournalOptions::sync_mode(SyncMode::Barrier)` — macOS `F_BARRIERFSYNC` opt-in (10–100× cheaper than `F_FULLFSYNC` on Apple Silicon NVMe) | macOS journal workloads on PLP-equipped enterprise NVMe with checkpoint discipline. *(0.9.4)* |
| **28** | [`28_write_lifetime_hint.rs`](../examples/28_write_lifetime_hint.rs) | `JournalOptions::write_lifetime_hint(WriteLifetimeHint::Long)` — Linux `F_SET_RW_HINT` for multi-stream NVMe GC clustering | Production WAL workloads on Linux multi-stream NVMe (2–5× GC write-amp reduction). *(0.9.4)* |

### WAL trim + reflink primitives (0.9.5–0.9.6)

| # | Example | What it shows | When to use this pattern |
|---|---|---|---|
| **21** | [`21_punch_hole_wal_trim.rs`](../examples/21_punch_hole_wal_trim.rs) | `Handle::punch_hole` / `write_zeros` — cross-platform sparse-file primitives | Database WAL trim post-checkpoint; log compaction; sparse file production. *(0.9.5)* |
| **29** | [`29_reflink_aware_copy.rs`](../examples/29_reflink_aware_copy.rs) | `Handle::copy` — instant CoW reflinks on APFS (clonefile) and ReFS (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`); silent fallback to bytewise elsewhere | Database checkpoint clones, container layering, backup tooling on APFS / ReFS. *(0.9.6)* |

## What's deliberately not included

- **`Method::Journal`** — reserved variant; selecting it returns `Error::UnsupportedMethod`. No example because there's no behaviour to demonstrate. For append-only workloads, use the [journal substrate](../examples/17_journal_basics.rs) instead.
- **A single "kitchen-sink" example** — reading 29 small focused examples is more useful than one large example that buries the concept under setup. If you want to see how the pieces fit together end-to-end, read `02_handle_basics.rs` then the one for the specific feature you need.
- **Linux btrfs / XFS reflink** — `29_reflink_aware_copy.rs` demonstrates the fast-path on APFS / ReFS; the Linux equivalent (`ioctl_ficlone` / `copy_file_range`) is not yet wired into `Handle::copy`. Tracked for a future release.

## See also

- [`API.md`](API.md) — full public-API surface.
- [`METHODS.md`](METHODS.md) — choosing the right `Method` for your workload.
- [`PERFORMANCE.md`](PERFORMANCE.md) — tuning knobs and their effect.
- [`PLATFORM-NOTES.md`](PLATFORM-NOTES.md) — per-OS quirks the examples don't cover in depth.
