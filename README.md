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

- **Explicit durability methods** &mdash; choose `Sync`, `Data`, `Direct`, or `Auto` instead of relying on implicit OS behavior.
- **Cross-platform IO semantics** &mdash; one API surface across Windows, Linux, and macOS, with platform-specific fallbacks documented rather than hidden.
- **Atomic replace-style writes** &mdash; file writes go through a temp-file and rename flow designed to avoid partial overwrite states.
- **Root-scoped handles** &mdash; bind a `Handle` to a base directory and reject paths that escape it.
- **File and directory CRUD** &mdash; write, read, append, positioned writes, range reads, copy, metadata, sync, directory creation/removal, listing, and existence checks.
- **Batch operations** &mdash; grouped writes, deletes, and copies through `write_batch`, `delete_batch`, `copy_batch`, and the chainable `Batch` builder.
- **Configurable group lane** &mdash; tune batch window, batch size, and queue depth per handle for throughput-oriented workloads.
- **Direct IO support with fallback** &mdash; attempts non-buffered/direct paths where supported, but degrades cleanly when a filesystem rejects them.
- **Quick one-shot API** &mdash; convenience helpers backed by a lazily initialized default handle for simple cases.
- **Structured error reporting** &mdash; explicit error variants for unsupported methods, alignment failures, atomic replace failures, and batch failure position.


&nbsp;

---

&nbsp;

&nbsp;


## What fsys provides today

- A `Handle` + `Builder` model for configuring IO once and reusing it across operations.
- Explicit durability methods: `Sync`, `Data`, `Direct`, and `Auto`.
- Cross-platform file CRUD: atomic replace-style writes, reads, appends, deletes, copies, range reads, metadata, and sync operations.
- Cross-platform directory CRUD: create, remove, recursive variants, existence checks, and listing.
- A root-scoped path model so a handle can enforce that all resolved paths stay under a chosen base directory.
- A convenience `quick` module for one-shot operations when you do not want to manage a handle directly.
- A batch API for grouped writes, deletes, and copies via `Handle::write_batch`, `Handle::delete_batch`, `Handle::copy_batch`, and the `Batch` builder.
- A per-handle group lane with bounded queueing and configurable batch thresholds for workloads that benefit from grouped dispatch.

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

Current state: **the core model is in place and the project is moving into its
next implementation phase**. The current release line is `0.5.x`.

Today, the crate already includes the handle/builder construction model,
cross-platform file and directory CRUD, explicit durability methods,
root-scoped path enforcement, quick helpers, and the batch/group-lane API.
That is enough to make the crate useful for early adopters evaluating the API
shape and operational model.

At the same time, some items are intentionally not available yet. In
particular, `Method::Mmap` and `Method::Journal` remain reserved variants, and
some platform-specific fast paths are still planned rather than implemented.
The roadmap below should be read as an engineering direction, not as a promise
that every label is complete simply because the version number has advanced.

- `0.1.x` — [**DONE**]: Initial setup.
- `0.2.x` — [**DONE**]: Scaffolding and foundation modules.
- `0.3.x` — [**DONE**]: Handle, CRUD, metadata, and cross-platform IO core.
- `0.4.x` — [**DONE**]: Group-lane batching and dispatcher pipeline.
- `0.5.x` — [**CURRENT**]: Consolidation of the new pipeline/core model and the next round of advanced IO paths.
- `0.6.x` — [**NEXT**]: Broader capability expansion, additional convenience APIs, and deeper platform optimization.
- `0.7.x` — [**ALPHA**]: `Method::Journal`, observability, deep audit.
- `0.8.x` — [**BETA**]: Public testing and compatibility validation.
- `0.9.x` — [**RC**]: Release candidate.
- `1.0.0` — Stable API release.

The roadmap is aspirational, not a schedule. Versions ship when they're right, not when the calendar agrees.

<hr><br>


## Installation

```toml
[dependencies]
fsys = "0.5.0"
```

> ⚠️ The crate is published and usable, but it should still be treated as pre-stable software. Use it in production only if you are comfortable tracking breaking changes before `1.0.0`.

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