<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  EXAMPLES
</h1>

The [`examples/`](../examples/) directory contains **17 runnable examples** covering every part of the public API. Each example is self-contained, comment-documented, and produces visible output so you can confirm the path you exercised.

> The 0.9.8 release is adding focused examples for the 0.9.1–0.9.7 surface additions (`append_batch`, `commit_grouped`, `tune_for(Workload::Database)`, `punch_hole`/`write_zeros`, `SyncMode::Barrier`, `dispatcher_shards`, `FsysObserver`, `sqpoll`, `is_plp_protected`, `atomic_write_unit`, `WriteLifetimeHint`). They land between this commit and the 0.9.8 tag; this doc updates as they merge.

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

## What's deliberately not included

- **`Method::Journal`** — reserved variant; selecting it returns `Error::UnsupportedMethod`. No example because there's no behaviour to demonstrate.
- **A single "kitchen-sink" example** — reading 17 small focused examples is more useful than one 800-line example that buries the concept under setup. If you want to see how the pieces fit together end-to-end, read `02_handle_basics.rs` then the one for the specific feature you need.

## See also

- [`API.md`](API.md) — full public-API surface.
- [`METHODS.md`](METHODS.md) — choosing the right `Method` for your workload.
- [`PERFORMANCE.md`](PERFORMANCE.md) — tuning knobs and their effect.
- [`PLATFORM-NOTES.md`](PLATFORM-NOTES.md) — per-OS quirks the examples don't cover in depth.
