<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  <sub>FILESYSTEM IO
</h1>
<p align="center">
  <strong>Durable Filesystem and Directory I/O for Rust</strong>
  <!--
  <strong>Adaptive File System &amp; Directory IO for Rust.</strong>
-->
</p>
<p align="center">
  <a href="https://crates.io/crates/fsys" alt="FSYS on Crates.io"><img alt="Crates.io" src="https://img.shields.io/crates/v/fsys"></a>
  <a href="https://crates.io/crates/fsys" alt="Download"><img alt="Crates.io Downloads" src="https://img.shields.io/crates/d/fsys?color=%230099ff"></a>
  <a href="https://docs.rs/fsys" title="Mod Events Documentation"><img alt="docs.rs" src="https://img.shields.io/docsrs/fsys"></a>
  <a href="https://github.com/jamesgober/fsys-rs/actions/workflows/ci.yml" title="CI status"><img alt="CI" src="https://github.com/jamesgober/fsys-rs/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <img alt="MSRV" src="https://img.shields.io/badge/msrv-1.75%2B-blue.svg?style=flat-square" title="Rust Version">
</p>

**FSYS** (`fsys-rs`) is a low-level file and directory IO crate for Rust.
It is aimed at systems code that needs explicit control over durability,
predictable cross-platform behavior, and an API surface that stays close to
how storage software actually thinks about IO.

The crate sits between `std::fs` and a fully bespoke platform layer. It keeps
the operational model explicit: you choose a durability method, build a
long-lived `Handle`, and issue file or directory operations through that
handle. On supported platforms, `fsys` uses the best available primitive for
the selected method while keeping fallback behavior visible rather than
implicit.

That makes it a good fit for storage engines, embedded databases, local-first
applications, durable caches, append-heavy services, background workers, and
other programs where write semantics matter as much as raw throughput. It is
not trying to replace `std::fs` for ordinary application code.

&nbsp;


## FEATURES

- **Journal substrate** &mdash; open-once append-only log file with atomic LSN reservation, group-commit fsync, and a CRC-32C-protected self-identifying frame format. Intended for write-ahead-log workloads (database WAL, persistent queues, ledgers) where the atomic-replace primitive's per-call fsync cost is the bottleneck. Three throughput tiers are present: a cross-platform synchronous core, a lock-free concurrent append path, and a native io_uring asynchronous substrate on Linux. An opt-in Direct-IO mode (`JournalOptions::direct(true)`) routes appends through a sector-aligned in-memory log buffer &mdash; the architecture used by InnoDB's redo log and the WiredTiger journal &mdash; which trades the lock-free hot path for predictable tail latency and zero-copy device writes via `O_DIRECT` / `F_NOCACHE` / `FILE_FLAG_NO_BUFFERING`. **0.9.1** adds a vectored `JournalHandle::append_batch(&[&[u8]])` that submits N records as a single framed-write syscall (~1.6× faster than `append`-in-loop on Windows page cache; larger wins expected on Linux + NVMe), hardware-accelerated CRC-32C with runtime CPU-feature dispatch (SSE4.2 / ARMv8 CRC), cache-padded hot atomics, stack-allocated frame encoding for small records, and a parking_lot Condvar leader/follower group-commit coordinator with two new tuning knobs (`JournalOptions::group_commit_window`, `group_commit_max_batch`) ported from emdb v0.8.5 (default `Some(500 µs)` / `8`).
- **Five real durability methods** &mdash; `Sync`, `Data`, `Mmap`, `Direct`, and hardware-aware `Auto`. Every method is platform-honest: the actual primitive in use is observable via `Handle::active_method()` and `Handle::active_durability_primitive()`.
- **Cross-platform IO semantics** &mdash; one API surface across Linux, macOS, and Windows, with platform-specific fallbacks documented rather than hidden.
- **NVMe passthrough flush** &mdash; on Linux (`NVME_IOCTL_IO_CMD`) and Windows (`IOCTL_STORAGE_PROTOCOL_COMMAND`) when the hardware supports it and the process has the privilege. Transparent fallback to `fdatasync` / `WRITE_THROUGH` otherwise.
- **Linux io_uring path** &mdash; `Method::Direct` on Linux routes through `io_uring` when available (kernel ≥ 5.1, no SECCOMP/AppArmor block), falling back to `O_DIRECT` + `pwrite` + `fdatasync` cleanly.
- **Atomic replace-style writes** &mdash; every public write API (`write`, `write_copy`, `write_batch`, `Batch::commit`) uses a temp-file + atomic rename pattern. The target file is either entirely the old payload or entirely the new payload &mdash; never torn.
- **Crash-safety verified** &mdash; per-method crash tests with three kill points (pre-syscall, mid-syscall, post-syscall) and the 100&times; pre-merge stability protocol.
- **`write_copy` with metadata preservation** &mdash; atomic-swap that preserves the target's existing mode (Unix), owner/group (Unix, when permitted), ACLs (Windows), and timestamps (all platforms).
- **Root-scoped handles** &mdash; bind a `Handle` to a base directory and reject paths that escape it.
- **Full file and directory CRUD** &mdash; write, read, append, positioned writes, range reads, truncate, rename, copy, metadata, sync, directory creation/removal, listing, recursive scan, glob find, and recursive count.
- **Batch operations** &mdash; grouped writes, deletes, and copies through `write_batch`, `delete_batch`, `copy_batch`, and the chainable `Batch` builder.
- **Async layer with two substrates** &mdash; gated behind the `async` Cargo feature. Every sync method gets an `_async` sibling. On Linux + `Method::Direct`, async ops submit directly to the per-handle io_uring ring (the **native substrate**, new in `0.7.0`). Everywhere else, async ops route through `tokio::task::spawn_blocking`. Which substrate a handle uses is observable via `Handle::async_substrate() -> AsyncSubstrate`.
- **Configurable group lane** &mdash; tune batch window, batch size, queue depth, io_uring queue depth, and aligned-buffer-pool size per handle.
- **Quick one-shot API** &mdash; convenience helpers backed by a lazily initialized default handle for simple cases.
- **Structured error reporting** &mdash; 21 explicit error variants with stable `FS-XXXXX` codes for unsupported methods, alignment failures, atomic-replace failures, NVMe passthrough denial, async-runtime requirements, glob-pattern errors, batch failure position, handle poisoning, io_uring submit failure, and completion-driver liveness.
- **Hardware-aware database surface (0.9.2)** &mdash; `Handle::is_plp_protected()` / `Handle::plp_status()` for safe per-commit fsync skip on confirmed-PLP enterprise NVMe (3&ndash;10&times; transaction-throughput lever); `crate::observer::FsysObserver` trait + `Builder::observer` for typed per-op telemetry (journal append / sync / handle write / read); runtime CPU-feature detection (replacing pre-0.9.2 compile-time `cfg!(target_feature = ...)` that lied on cross-target builds); `Builder::tune_for(Workload::Database)` for one-line storage-engine tuning (8 MiB buffer pool, 256-deep io_uring ring, 4096-deep batch queue).
- **Pipeline throughput tier (0.9.3)** &mdash; `Builder::dispatcher_shards(N)` spawns N independent dispatcher threads per handle, each with its own bounded queue; batches hash-routed by first op's path so within-batch order is preserved while concurrent submitters writing to different files scale near-linearly with shard count (was a one-core ceiling pre-0.9.3). `Batch::commit_grouped()` amortises parent-directory `fsync` across the entire batch &mdash; one syscall per unique parent directory instead of one per op &mdash; for bulk-load / SST-flush / checkpoint workloads where the batch is the durability unit.
- **io_uring elite &mdash; Linux (0.9.4)** &mdash; process-cached kernel-feature probe applies `IORING_SETUP_COOP_TASKRUN` / `SINGLE_ISSUER` / `DEFER_TASKRUN` to every io_uring ring fsys constructs (kernel &ge; 5.19 / 6.0 / 6.1 respectively, with graceful downgrade on older kernels); linked `Write + Fsync(DATASYNC)` via `IOSQE_IO_LINK` halves the durable-write syscall round-trip on the atomic-replace Direct path; NAWUN / NAWUPF probe via NVMe Identify Namespace exposes `Handle::atomic_write_unit() -> Option<u32>` so databases on guaranteeing drives can safely skip torn-write detection on writes up to that size. Linux-only paths are `#[cfg(target_os = "linux")]`-gated.
- **Cross-platform sync tuning (0.9.4)** &mdash; `JournalOptions::sync_mode(SyncMode::Barrier)` opts into macOS `F_BARRIERFSYNC` (10&ndash;100&times; cheaper than `F_FULLFSYNC` on Apple Silicon NVMe; crash-safe **only** on PLP drives or under explicit eventual-`Full`-sync discipline). `JournalOptions::write_lifetime_hint(Some(WriteLifetimeHint::Long))` applies the Linux `F_SET_RW_HINT` fcntl so multi-stream NVMe drives cluster long-lived journal data into separate NAND blocks, reducing GC write amplification. Both knobs default off; pre-0.9.4 behaviour preserved exactly.
- **Performance + IO tuning (0.9.5)** &mdash; **dual-buffered Direct-mode log buffer** decouples appends from in-flight flushes so concurrent writers no longer block on the `write_at_direct` syscall (Direct mode goes from a single-core ceiling to multi-core scalable on HiveDB-class workloads). `Handle::punch_hole(path, offset, len)` and `Handle::write_zeros(path, offset, len)` expose cross-platform sparse-file primitives backed by Linux `fallocate(FALLOC_FL_PUNCH_HOLE | FL_ZERO_RANGE)`, macOS `fcntl(F_PUNCHHOLE)`, and Windows `FSCTL_SET_ZERO_DATA` &mdash; the WAL-trim primitive databases use to give back consumed segments without touching the page cache. Both Linux io_uring rings (sync owner-thread + async substrate) now use `IORING_REGISTER_FILES` so per-op `fd`s are lazily upgraded to fixed-file slots, eliminating per-SQE kernel-side fd validation on the hot path.
- **0.9.6 audit + journal-on-io_uring + reflinks** &mdash; full-codebase audit pass with **38 findings** triaged (all 5 CRITICAL resolved; 13 of 16 HIGH resolved with the others deferred to documented architectural-dep follow-ups). Architectural centerpiece: the Direct-mode journal flush path now routes through **`IORING_OP_WRITE_FIXED`** against pre-registered AlignedBuf slots on Linux (silent fallback to `pwrite` elsewhere); macOS `clonefile(2)` and Windows `FSCTL_DUPLICATE_EXTENTS_TO_FILE` give APFS/ReFS reflink semantics for `copy_file`; real OS-version probes via `sysctlbyname` (macOS) and `RtlGetVersion` (Windows); real page-size probe via `sysconf`/`GetSystemInfo`. CI hardening: feature-matrix job + `cargo audit`/`cargo deny` daily workflow + 60-second soak in every PR.


&nbsp;

<hr><br>


## Installation

```toml
[dependencies]
fsys = "0.9.6"
```

To opt into the async layer:

```toml
[dependencies]
fsys = { version = "0.9.6", features = ["async"] }
```

<br>

### Cargo features

| Feature | Default | Pulls in | Purpose |
|---|---|---|---|
| `async` | off | `tokio` (`rt`, `rt-multi-thread`, `sync`, `macros`) | `_async` siblings for every sync method; async batch via `tokio::sync::oneshot`. |
| `stress` | off | (none) | Switches the soak tests in `tests/stress.rs` from a 60-second validation run to the full 1-hour soak duration. CI nightly enables this; dev iteration leaves it off. |
| `fuzz` | off | (none) | Compile-only flag for fuzz instrumentation. The actual fuzz targets live in `fuzz/` (separate `cargo-fuzz` workspace). |

<br>

### Minimum supported Rust version

`1.75`. The MSRV may be raised in any minor version before `1.0.0`. After `1.0.0`, MSRV bumps require a minor version bump.

<br>

### Benchmark results

Numbers below were captured on `windows-ntfs-nvme` (Windows 11 Pro, x86_64, local NVMe SSD; `std::env::temp_dir()` resolves to NTFS) with 100 timed iterations after 10 warmup. Run-to-run noise is roughly &plusmn;5 % on this host class. The full methodology, additional payload sizes, and Linux numbers live in [`docs/BENCH.md`](docs/BENCH.md); reproduce locally with `cargo bench`.

**Journal substrate vs atomic-replace** &mdash; the headline 0.9.0 result. Atomic-replace pays 5&ndash;7 syscalls per durable write; the journal opens once, appends without per-call fsync, and amortises durability across a `sync_through` call.

| Payload | Atomic-replace | Journal (sync at end) | Speedup |
|---------|---------------:|----------------------:|--------:|
| 64 B | 634 ops/s | 462.9 K ops/s | **730&times;** |
| 4 KiB | 891 ops/s | 189.3 K ops/s | **212&times;** |

The "sync at end" cadence is the canonical WAL pattern: append many records, fsync once at a transaction boundary. At an intermediate cadence (sync every 100 appends), the journal still delivers 109&ndash;255&times; the atomic-replace throughput. See [`docs/BENCH.md`](docs/BENCH.md#journal-substrate-090-r-1--vs-atomic-replace) for the full table including the per-append sync cadence.

**Atomic-replace `write` vs `std::fs::write`** &mdash; tail latency is what fsys pays for; medians on small writes go to `std::fs::write` because it does not provide durability guarantees.

| Payload | `fsys::Auto` median / p99 | `std::fs::write` median / p99 |
|---------|--------------------------:|------------------------------:|
| 4 KiB | 1.08 ms / 4.69 ms | 218.7 &micro;s / 7.18 ms |
| 64 KiB | 1.23 ms / 5.50 ms | 4.48 ms / 5.47 ms |
| 1 MiB | 1.80 ms / 5.00 ms | 2.84 ms / 16.45 ms |

`std::fs::write` is ~5&times; faster than `fsys::Auto` at the 4 KiB median because it skips the `fsync` + atomic-rename cycle. At p99 the gap inverts: `fsys::Auto` is 3.3&times; faster than `std::fs::write` at 1 MiB because the durability cost is paid deterministically rather than deferred to OS scheduling. The fair comparison for durable writes is `fsys::Sync` versus `std::fs` plus a manual temp-file + `sync_all` + `rename` dance &mdash; the latter is what most application code gets wrong.

**Read parity** &mdash; the read path is essentially `std::fs::read` plus handle bookkeeping.

| Payload | `fsys::Auto` median / p99 | `std::fs::read` median / p99 | `tokio::fs::read` median / p99 |
|---------|--------------------------:|-----------------------------:|-------------------------------:|
| 4 KiB | 25.0 / 89.4 &micro;s | 23.7 / 77.1 &micro;s | 35.8 / 152.8 &micro;s |
| 64 KiB | 25.0 / 58.9 &micro;s | 24.1 / 64.0 &micro;s | 105.9 / 337.5 &micro;s |
| 1 MiB | 182.5 / 482.3 &micro;s | 189.0 / 327.4 &micro;s | 250.7 / 585.8 &micro;s |

`tokio::fs::read` (simulated via `spawn_blocking`, which is what tokio's own `fs` module does internally) is 1.5&ndash;4.4&times; slower because of the thread-pool hop. On Linux + `Method::Direct` + the `async` feature, `fsys`'s native io_uring substrate bypasses that hop entirely &mdash; see [`docs/BENCH.md`](docs/BENCH.md) for the WSL2 measurement.

<br>

### Documentation

- API reference: <https://docs.rs/fsys>
- Architecture overview: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- **Runnable examples (16)**: [`docs/EXAMPLES.md`](docs/EXAMPLES.md) &mdash; catalogues every example in [`examples/`](examples/) with a "when to use this pattern" guide.
- Method matrix and `Auto` decision ladder: [`docs/METHODS.md`](docs/METHODS.md)
- Performance targets and tuning: [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)
- Crash-safety contract per method: [`docs/CRASH-SAFETY.md`](docs/CRASH-SAFETY.md)
- Per-platform behavior + capability requirements: [`docs/PLATFORM-NOTES.md`](docs/PLATFORM-NOTES.md)
- API stability + breaking-change policy: see *Stability + breaking-change policy* and *API changes in 0.9.0* in [`docs/API.md`](docs/API.md). Per-version migration deltas live in [`CHANGELOG.md`](CHANGELOG.md).



<!-- CONTRIBUTORS
---------------------------------->
<br><br>
<h2 align="center">CONTRIBUTORS</h2>

Coming Soon...


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