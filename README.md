<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS</code>
  <br>
  <sub>FILE SYSTEM IO
</h1>
<p align="center">
  <strong>Adaptive File System &amp; Directory IO for Rust.</strong>
</p>
<p align="center">
  <a href="https://crates.io/crates/fsys" alt="FSYS on Crates.io"><img alt="Crates.io" src="https://img.shields.io/crates/v/fsys"></a>
  <a href="https://crates.io/crates/fsys" alt="Download"><img alt="Crates.io Downloads" src="https://img.shields.io/crates/d/fsys?color=%230099ff"></a>
  <a href="https://docs.rs/fsys" title="Mod Events Documentation"><img alt="docs.rs" src="https://img.shields.io/docsrs/fsys"></a>
  <a href="https://github.com/jamesgober/fsys-rs/actions/workflows/ci.yml" title="CI status"><img alt="CI" src="https://github.com/jamesgober/fsys-rs/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <img alt="MSRV" src="https://img.shields.io/badge/rustc-1.75%2B-blue.svg?style=flat-square">
</p>


**FSYS** (`fsys-rs`) is a low-level filesystem abstraction for Rust, designed for storage engines, databases, and any application that needs predictable, high-performance file IO with explicit control over durability.

It exists because the standard library's file IO is general-purpose by design, it has to work everywhere, behave reasonably on every platform, and never surprise a casual user. Storage engines need the opposite: they need to know exactly what's happening, pick the fastest strategy their hardware allows, and pay zero cost for features they don't use.


---

> **Status:** Early development. The public API is not yet stable. Expect breaking changes until `0.8.0`.

&nbsp;

## Design goals

- **Hardware-aware.** Detect drive type (NVMe, SSD, HDD) and capabilities (Power Loss Protection, queue depth, optimal block size) at startup. Pick defaults that match the device.
- **Multi-strategy durability.** Support `fsync`, `fdatasync`, NVMe passthrough flush, and Phantom Buffer modes. The caller picks the tradeoff; `fsys` doesn't decide for them.
- **Adaptive dispatch.** Route single writes through a low-latency solo lane and bulk writes through a group-commit lane. The dispatcher switches automatically based on load.
- **Cross-platform.** Linux, macOS, Windows. Best available path on each, with honest fallbacks where the OS doesn't expose the primitive.
- **Zero magic.** Every strategy is explicit. No hidden buffering, no unexpected flushes, no surprises in the latency tail.

## Non-goals

- High-level filesystem features (locking semantics, watch APIs, path manipulation).
- Replacing `std::fs` for general use.
- Async-runtime integration in the core. Async wrappers may live in a separate crate.


## Status & roadmap

Current state: **dual-pipeline model in place**. `0.4.0` adds the group-lane
batch API on top of the `0.3.0` foundation: `Handle::write_batch`,
`delete_batch`, `copy_batch`, and the chainable `Batch` builder route through
a per-handle dispatcher thread (lazy spawn, clean shutdown on drop) with a
hybrid time-or-count window and a bounded queue with blocking submission.
Solo-lane operations (`write`, `read`, `append`, `write_at`, etc.) remain
byte-for-byte identical to `0.3.0` — the pipeline is invisible on that path.

- `0.1.x` — [**DONE**]: Initial setup.
- `0.2.x` — [**DONE**]: Scaffolding and foundation modules.
- `0.3.x` — [**DONE**]: Construction core (CRUD + Handle + cross-platform IO).
- `0.4.x` — [**CURRENT**]: Pipelines + batch operations (group-lane dispatch, `Batch` builder, `BatchError`).
- `0.5.x` — [**NEXT**]: Real hardware probe + `Method::Mmap` / `Direct` consolidation, Linux `io_uring`, NVMe passthrough.
- `0.6.x` — [**NEXT**]: `write_copy`, async batch wrappers, `scan` / `find` / `count` directory APIs.
- `0.7.x` — [**ALPHA**]: `Method::Journal`, observability, deep audit.
- `0.8.x` — [**BETA**]: Public testing and compatibility validation.
- `0.9.x` — [**RC**]: Release candidate.
- `1.0.0` — Stable API release.

The roadmap is aspirational, not a schedule. Versions ship when they're right, not when the calendar agrees.

<hr><br>


## Installation

```toml
[dependencies]
fsys = "0.4.0"
```

> The crate is published to reserve the name. **Do not depend on it for production work** until at least `0.8.0`.

<br>

### Minimum supported Rust version

`1.75`. The MSRV may be raised in any minor version before `1.0.0`. After `1.0.0`, MSRV bumps require a minor version bump.



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