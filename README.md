<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  <sub>FILESYSTEM IO
</h1>
<p align="center">
  <strong>Durable filesystem IO for Rust storage engines.</strong>
</p>
<p align="center">
  <a href="https://crates.io/crates/fsys" alt="FSYS on Crates.io"><img alt="Crates.io" src="https://img.shields.io/crates/v/fsys"></a>
  <a href="https://crates.io/crates/fsys" alt="Download"><img alt="Crates.io Downloads" src="https://img.shields.io/crates/d/fsys?color=%230099ff"></a>
  <a href="https://docs.rs/fsys" title="API documentation on docs.rs"><img alt="docs.rs" src="https://img.shields.io/docsrs/fsys"></a>
  <a href="https://github.com/jamesgober/fsys-rs/actions/workflows/ci.yml" title="CI status"><img alt="CI" src="https://github.com/jamesgober/fsys-rs/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <img alt="MSRV" src="https://img.shields.io/badge/msrv-1.75%2B-blue.svg?style=flat-square" title="Rust Version">
</p>

`fsys` is a foundation-tier filesystem IO crate for Rust storage engines, embedded databases, and durable services. It pairs an explicit durability model with a journal substrate, io_uring on Linux, NVMe passthrough, and atomic-replace writes &mdash; sitting one layer below your data structures and one layer above `std::fs`.

It is not trying to replace `std::fs` for ordinary application code.

&nbsp;

## Quickstart

```rust
use std::sync::Arc;
use fsys::{builder, JournalOptions};

fn main() -> fsys::Result<()> {
    // Build a handle once, share via Arc.
    let fs = Arc::new(builder().build()?);

    // Open an append-only journal &mdash; the WAL primitive.
    let log = fs.journal("/var/lib/myapp/log.wal")?;

    // Append many records without per-call fsync.
    let _ = log.append(b"txn 1: insert")?;
    let _ = log.append(b"txn 2: update")?;
    let lsn = log.append(b"txn 3: commit")?;

    // One fsync covers every prior append &mdash; group-commit.
    log.sync_through(lsn)?;

    Ok(())
}
```

For one-shot file IO (atomic-replace, durable), `fsys::quick::write` / `read` skip the handle:

```rust
fsys::quick::write("/etc/myapp/config.toml", b"value = 42")?;
let data = fsys::quick::read("/etc/myapp/config.toml")?;
```

See [`examples/`](examples/) (17 runnable patterns) and [`docs/EXAMPLES.md`](docs/EXAMPLES.md) for the full catalogue.

&nbsp;

## At a glance

- **Five durability methods** &mdash; `Sync`, `Data`, `Mmap`, `Direct`, and hardware-aware `Auto`. Every method is platform-honest: the actual primitive in use is observable via `Handle::active_durability_primitive()`.
- **Journal substrate** &mdash; open-once append-only log with atomic LSN reservation, group-commit fsync, and a CRC-32C-protected frame format. Three throughput tiers (sync, lock-free concurrent, native io_uring async on Linux). The HiveDB-class WAL primitive.
- **Atomic-replace writes** &mdash; every `write` / `write_copy` / `Batch::commit` uses temp-file + atomic rename. The target is either entirely the old payload or entirely the new payload &mdash; never torn.
- **Linux io_uring on the hot path** &mdash; `Method::Direct` and the journal Direct-IO path submit through io_uring with `IORING_OP_WRITE_FIXED` against pre-registered buffer slots. Falls back to `O_DIRECT` + `pwrite` + `fdatasync` cleanly when io_uring is unavailable.
- **NVMe passthrough flush** &mdash; on Linux (`NVME_IOCTL_IO_CMD`) and Windows (`IOCTL_STORAGE_PROTOCOL_COMMAND`) when the hardware supports it. Transparent fallback to `fdatasync` / `WRITE_THROUGH` otherwise.
- **Cross-platform reflinks** &mdash; macOS `clonefile(2)` + Windows `FSCTL_DUPLICATE_EXTENTS_TO_FILE` give APFS / ReFS instant copy-on-write semantics. Multi-GiB checkpoint clones drop from seconds to microseconds.
- **Optional async layer** (`async` feature) &mdash; every sync method gets an `_async` sibling. On Linux + `Method::Direct`, async ops submit directly to the per-handle io_uring ring (no `spawn_blocking` thread-pool hop).
- **Hardware-aware tuning** &mdash; PLP detection, NAWUN/NAWUPF probe (atomic-write unit), `Builder::tune_for(Workload::Database)` preset, runtime CPU-feature detection for hardware CRC-32C.

&nbsp;

## When to use `fsys`

| You need... | Use |
|---|---|
| A casual file read or write in a non-critical path | [`std::fs`](https://doc.rust-lang.org/std/fs/) |
| Async file IO inside a tokio program, no durability requirements | [`tokio::fs`](https://docs.rs/tokio/latest/tokio/fs/) (which routes through `spawn_blocking`) |
| **A durable write that survives `kill -9`** | `fsys` &mdash; atomic-replace pattern |
| **A write-ahead log / WAL / journal** | `fsys::JournalHandle` |
| **Direct-IO on NVMe with explicit fsync control** | `fsys::Handle` with `Method::Direct` |
| **One Rust crate that handles Linux + macOS + Windows durability cleanly** | `fsys` &mdash; per-platform fallback ladder, observable via `Handle::active_durability_primitive()` |
| The lowest possible `std::fs::write` latency in the *happy path* | `std::fs::write` (skips `fsync`, doesn't survive crash) |

The "fair comparison" for durable writes is `fsys::Sync` versus `std::fs` plus a manual temp-file + `sync_all` + `rename` dance &mdash; the latter is what most application code gets wrong. `fsys` provides this as a single public API call.

&nbsp;

## Performance

Numbers below were captured on `windows-ntfs-nvme` (Windows 11 Pro, x86_64, local NVMe SSD; `std::env::temp_dir()` resolves to NTFS) with 100 timed iterations after 10 warmup. Run-to-run noise is roughly &plusmn;5% on this host class. Full methodology, additional payload sizes, and Linux numbers live in [`docs/BENCH.md`](docs/BENCH.md); reproduce locally with `cargo bench`.

### Journal substrate vs atomic-replace

The headline result. Atomic-replace pays 5&ndash;7 syscalls per durable write; the journal opens once, appends without per-call fsync, and amortises durability across a `sync_through` call &mdash; the canonical WAL pattern.

| Payload | Atomic-replace | Journal (sync at end) | Speedup |
|---------|---------------:|----------------------:|--------:|
| 64 B | 634 ops/s | 462.9 K ops/s | **730&times;** |
| 4 KiB | 891 ops/s | 189.3 K ops/s | **212&times;** |

At an intermediate cadence (sync every 100 appends), the journal still delivers 109&ndash;255&times; the atomic-replace throughput. See [`docs/BENCH.md`](docs/BENCH.md) for the full table including per-append sync cadence.

### Atomic-replace `write` vs `std::fs::write`

`fsys::Auto` pays a deterministic durability cost; `std::fs::write` defers that cost to OS scheduling and pays it at p99 instead.

| Payload | `fsys::Auto` median / p99 | `std::fs::write` median / p99 |
|---------|--------------------------:|------------------------------:|
| 4 KiB | 1.08 ms / 4.69 ms | 218.7 &micro;s / 7.18 ms |
| 64 KiB | 1.23 ms / 5.50 ms | 4.48 ms / 5.47 ms |
| 1 MiB | 1.80 ms / 5.00 ms | 2.84 ms / 16.45 ms |

At 1 MiB, `fsys::Auto` is **3.3&times; faster than `std::fs::write` at p99** &mdash; durability paid up-front rather than at unpredictable points.

### Read parity

The read path is essentially `std::fs::read` plus handle bookkeeping &mdash; no durability cost on reads.

| Payload | `fsys::Auto` median / p99 | `std::fs::read` median / p99 | `tokio::fs::read` median / p99 |
|---------|--------------------------:|-----------------------------:|-------------------------------:|
| 4 KiB | 25.0 / 89.4 &micro;s | 23.7 / 77.1 &micro;s | 35.8 / 152.8 &micro;s |
| 64 KiB | 25.0 / 58.9 &micro;s | 24.1 / 64.0 &micro;s | 105.9 / 337.5 &micro;s |
| 1 MiB | 182.5 / 482.3 &micro;s | 189.0 / 327.4 &micro;s | 250.7 / 585.8 &micro;s |

`tokio::fs::read` is 1.5&ndash;4.4&times; slower than `fsys::Auto` because tokio's own `fs` module routes through `spawn_blocking`. On Linux + `Method::Direct` + the `async` feature, `fsys`'s native io_uring substrate bypasses that thread-pool hop entirely.

&nbsp;

## Installation

```toml
[dependencies]
fsys = "1.0"
```

With the async layer:

```toml
[dependencies]
fsys = { version = "1.0", features = ["async"] }
```

### Cargo features

| Feature | Default | Pulls in | Purpose |
|---|---|---|---|
| `async` | off | `tokio` (`rt`, `rt-multi-thread`, `sync`, `macros`) | `_async` siblings for every sync method; async batch via `tokio::sync::oneshot`. |
| `tracing` | off | `tracing` | Structured spans + events on the write / read / journal hot paths. No-op when subscriber is absent. |
| `stress` | off | (none) | Switches `tests/stress.rs` from a 60-second validation run to the full 1-hour soak. CI nightly enables this; dev iteration leaves it off. |
| `fuzz` | off | (none) | Compile-only flag for fuzz instrumentation. Actual targets live in `fuzz/` (cargo-fuzz workspace). |

### Minimum supported Rust version

`1.75`. Through the `1.x` line, MSRV bumps are allowed only in `1.x.0` minor releases (within the 12 most recent stable Rust versions at release time). Patch releases never bump MSRV. See [`docs/STABILITY-1.0.md`](docs/STABILITY-1.0.md) for the full policy.

&nbsp;

## Highlights by release

The full per-version delta lives in [`CHANGELOG.md`](CHANGELOG.md). Headline capabilities by release:

| Release | Headline |
|---|---|
| **1.0.0** | First stable release. SemVer + on-disk-format guarantees apply for the `1.x` line per [`docs/STABILITY-1.0.md`](docs/STABILITY-1.0.md). No source-logic changes vs. `0.9.8`. |
| **0.9.8** | Final pre-1.0 polish: documentation refresh, examples expansion, canonical benchmarks, `STABILITY-1.0.md` commitment doc. |
| **0.9.7** | GroupCommit wake-stampede fix (atomic `pending_followers`, ~5&times; lock-hold reduction under 100+ followers); `Builder::sqpoll(idle_ms)` opt-in kernel-side submission polling; `IORING_REGISTER_FILES` restored on both rings; OOM-injection test infrastructure; LSN atomic-ordering tightened to `Release`. |
| **0.9.6** | Full-codebase audit (38 findings); journal-on-io_uring via `IORING_OP_WRITE_FIXED`; APFS `clonefile(2)` + ReFS `FSCTL_DUPLICATE_EXTENTS_TO_FILE` reflinks for `copy_file`; real OS-version probes; `Lsn` + `BatchError` field lockdown for pre-1.0 stability. |
| **0.9.5** | Dual-buffered Direct-mode log buffer (multi-core scalable journal appends); `Handle::punch_hole` + `Handle::write_zeros` cross-platform sparse-file primitives; `IORING_REGISTER_FILES` on both io_uring rings. |
| **0.9.4** | io_uring elite flags (`COOP_TASKRUN` / `SINGLE_ISSUER` / `DEFER_TASKRUN`); linked Write+Fsync via `IOSQE_IO_LINK`; NAWUN / NAWUPF probe and `Handle::atomic_write_unit()`; macOS `SyncMode::Barrier` for `F_BARRIERFSYNC`; Linux `WriteLifetimeHint` for multi-stream NVMe. |
| **0.9.3** | `Builder::dispatcher_shards(N)` for multi-core batch throughput; `Batch::commit_grouped()` amortises parent-directory fsync. |
| **0.9.2** | PLP detection (`Handle::is_plp_protected` / `plp_status`); `FsysObserver` trait + `Builder::observer` for telemetry; `Builder::tune_for(Workload::Database)`; runtime CPU-feature detection for hardware CRC-32C. |
| **0.9.1** | Vectored `JournalHandle::append_batch(&[&[u8]])` (~1.6&times; faster than `append`-in-loop on Windows NTFS, larger wins on Linux NVMe); hardware-accelerated CRC-32C (SSE4.2 / ARMv8 CRC); cache-padded hot atomics; group-commit window + max-batch tuning. |
| **0.9.0** | Journal substrate (three throughput tiers); Direct-IO journal opt-in; CRC-32C frame format with tail-truncation detection; per-method crash-safety integration tests. |

&nbsp;

## Documentation

- **API reference**: <https://docs.rs/fsys>
- **17 runnable examples**: [`docs/EXAMPLES.md`](docs/EXAMPLES.md) &mdash; catalogues every example in [`examples/`](examples/) with a "when to use this pattern" guide.
- **Architecture overview**: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- **Method matrix + `Auto` decision ladder**: [`docs/METHODS.md`](docs/METHODS.md)
- **Performance targets + tuning**: [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)
- **Crash-safety contract per method**: [`docs/CRASH-SAFETY.md`](docs/CRASH-SAFETY.md)
- **Per-platform behavior + capability requirements**: [`docs/PLATFORM-NOTES.md`](docs/PLATFORM-NOTES.md)
- **Benchmark methodology + results**: [`docs/BENCH.md`](docs/BENCH.md)
- **Public-API reference**: [`docs/API.md`](docs/API.md)
- **Per-version migration deltas**: [`CHANGELOG.md`](CHANGELOG.md)


<!-- LICENSE
---------------------------------->
<br><h2 id="license" align="center">LICENSE</h2>

Licensed under the **Apache License version 2.0** [ [LICENSE-APACHE](./LICENSE-APACHE) ], or the **MIT License** [ [LICENSE-MIT](./LICENSE-MIT) ]; otherwise known as the (**"`License Agreement`"**); you are permitted to use this software, its source code, documentation, concepts, and any of the associated contents, within the limitations defined by the **"`License Agreement`"**.

<div align="center">
  <a href="https://www.apache.org/licenses/LICENSE-2.0" title="Apache License - version 2.0">https://www.apache.org/licenses/LICENSE-2.0</a><br>
  <a href="https://opensource.org/licenses/MIT" title="MIT License">https://opensource.org/licenses/MIT</a>
</div>


<!-- COPYRIGHT
---------------------------------->
<div align="center">
  <br>
  <h2></h2>
  Copyright &copy; 2026 James Gober.
</div>
