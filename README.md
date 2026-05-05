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
  <img alt="MSRV" src="https://img.shields.io/badge/rustc-1.75%2B-blue.svg?style=flat-square">
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


---

> ⚠️ **Status:** Early development.  
The crate is usable and tested, but the API is not stable yet. Expect breaking changes before `1.0.0`.

&nbsp;


## FEATURES

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


&nbsp;

---

&nbsp;

&nbsp;


## What fsys provides today

- A `Handle` + `Builder` model for configuring IO once and reusing it across operations.
- Five durability methods: `Sync`, `Data`, `Mmap`, `Direct` (with NVMe passthrough on capable hardware), and `Auto`.
- Cross-platform file CRUD: atomic replace-style writes, `write_copy` (atomic-swap with metadata preservation), reads, appends, deletes, copies, range reads, truncate, rename, metadata, and sync operations.
- Cross-platform directory CRUD: create, remove, recursive variants, existence checks, listing, recursive scan, glob find, and recursive count.
- A root-scoped path model so a handle can enforce that all resolved paths stay under a chosen base directory.
- A convenience `quick` module for one-shot operations when you do not want to manage a handle directly.
- A batch API for grouped writes, deletes, and copies via `Handle::write_batch`, `Handle::delete_batch`, `Handle::copy_batch`, and the `Batch` builder.
- A per-handle group lane with bounded queueing and configurable batch thresholds for workloads that benefit from grouped dispatch.
- An optional async layer (feature `async`) covering every sync method.
- The `fsys::primitive` module of canonical durability-primitive strings for runtime observation of the active code path.

## Design principles

- **Explicit semantics first.** The API should make durability and routing choices visible.
- **Portable without pretending platforms are identical.** Linux, macOS, and Windows have different primitives; `fsys` exposes one model while documenting where fallbacks occur.
- **Low ceremony for the common path.** A single `Handle` should cover most workloads; the `quick` module exists for one-off use.
- **Predictable failure behavior.** Atomic write/replace and batch operations report where failure happened instead of silently smoothing over it.
- **Room to grow.** Reserved methods and pipeline internals make it possible to expand into more advanced IO paths without replacing the public model.

## When to use fsys

Use `fsys` when you need one or more of the following:

- Explicit control over full sync, data-only sync, direct IO, or automatic method selection.
- Cross-platform file IO behavior that is stricter and more deliberate than `std::fs` defaults.
- Atomic replace-style writes for file updates.
- A handle-scoped root for keeping file activity inside a known subtree.
- Grouped write/delete/copy submission for higher-throughput batch-style work.

## Non-goals

- High-level filesystem tooling such as watchers, lock orchestration, virtual filesystems, or rich path-manipulation utilities.
- Replacing `std::fs` for everyday scripting or application scaffolding.
- Hiding platform-specific tradeoffs behind vague “fast mode” abstractions.
- Full async-runtime integration in the core crate.


## Status & roadmap

Current state: **the public API is feature-complete for `1.0` and frozen at
the upcoming `0.8.0` alpha tag.** From `0.8.0` onward, only genuine bugs may
change a name or signature. The current release line is `0.7.x`, which is
the last breaking-change phase before alpha.

`0.7.0` shipped the optimization phase: the **native io_uring async
substrate** on Linux (`Method::Direct` + `async` feature) which submits
directly to the per-handle ring instead of hopping `spawn_blocking`,
the [`AsyncSubstrate`] enum + `Handle::async_substrate()` accessor, the
`FSYS_DISABLE_NATIVE_ASYNC=1` environment override, PLP detection
refinement (per-vendor enterprise-NVMe lookup table), 3 new error variants
(`HandlePoisoned`, `IoUringSubmitFailed`, `CompletionDriverDead`), and the
public-API audit pass that tightened 5 names (`read_range` &rarr; `read_at`,
`scan(_, recursive)` split into `scan` + `scan_all`, same for `count`,
`buffer_pool_size` &rarr; `buffer_pool_count`,
`buffer_pool_block` &rarr; `buffer_pool_block_size`).

`0.6.0` finished the rest of the public surface: the async layer, NVMe
passthrough flush on Linux and Windows, completion CRUD methods
(`write_copy`, `scan`, `find`, `count`), `Handle::active_durability_primitive()`
plus the `fsys::primitive` constants module, and a publication-quality
documentation pass across the crate. The `0.5.x` line consolidated real
hardware probing, the real `Method::Mmap` implementation, the io_uring
path on Linux, and the per-method crash-test harness. Everything from
earlier phases (handle/builder, full file/dir CRUD, batch + group lane)
is unchanged.

What remains before `1.0`:

- API freeze at the `0.8.0` alpha tag.
- Tier-3 24-hour soak certification on representative hardware classes.
- Bare-metal Linux native-substrate measurement (formalising the 2&times;+
  expectation behind the 1.46&times; figure measured in WSL2 + ext4).
- Crash-safety certification with forced-unmount sim.
- Full alpha &rarr; beta &rarr; RC progression.

`Method::Journal` (intent-log durability) is **deferred indefinitely**.
The variant is kept in the public API as a forward-compatibility
placeholder, but no version commits to implementing it; it needs its own
multi-phase design pass that the alpha freeze takes priority over.

The roadmap below shows where each phase landed.

- `0.1.x` &mdash; [**DONE**]: Initial setup.
- `0.2.x` &mdash; [**DONE**]: Scaffolding and foundation modules.
- `0.3.x` &mdash; [**DONE**]: Handle, CRUD, metadata, and cross-platform IO core.
- `0.4.x` &mdash; [**DONE**]: Group-lane batching and dispatcher pipeline.
- `0.5.x` &mdash; [**DONE**]: Real hardware probe, `Method::Mmap`, `Method::Direct` with io_uring on Linux, per-method crash tests.
- `0.6.x` &mdash; [**DONE**]: Async layer, NVMe passthrough, completion CRUD, publication-quality docs.
- `0.7.x` &mdash; [**CURRENT**]: Native io_uring async substrate, PLP refinement, API audit + cleanup, regression suite, optimization phase.
- `0.8.x` &mdash; [**NEXT (alpha + freeze)**]: API freeze. Tier-3 soak certification, bare-metal native-substrate measurement, crash-safety certification.
- `0.9.x` &mdash; [**RC**]: Release candidate.
- `1.0.0` &mdash; Stable API release.

The roadmap is aspirational, not a schedule. Versions ship when they're right, not when the calendar agrees.

<hr><br>


## Installation

```toml
[dependencies]
fsys = "0.7.0"
```

To opt into the async layer:

```toml
[dependencies]
fsys = { version = "0.7.0", features = ["async"] }
```

> ⚠️ The crate is published and usable, but it should still be treated as pre-stable software. Use it in production only if you are comfortable tracking breaking changes before `1.0.0`.

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

### Documentation

- API reference: <https://docs.rs/fsys>
- Architecture overview: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- Method matrix and `Auto` decision ladder: [`docs/METHODS.md`](docs/METHODS.md)
- Performance targets and tuning: [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)
- Crash-safety contract per method: [`docs/CRASH-SAFETY.md`](docs/CRASH-SAFETY.md)
- Per-platform behavior + capability requirements: [`docs/PLATFORM-NOTES.md`](docs/PLATFORM-NOTES.md)
- API stability + breaking-change policy: see *Stability + breaking-change policy* and *API changes in 0.7.0* in [`docs/API.md`](docs/API.md). Per-version migration deltas live in [`CHANGELOG.md`](CHANGELOG.md).



<!-- CONTRIBUTORS
---------------------------------->
<br><br>
<h2 align="center">CONTRIBUTORS</h2>

Coming Soon


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