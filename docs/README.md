<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  DOCUMENTATION
</h1>

The `docs/` tree is the long-form companion to the rustdoc API reference at <https://docs.rs/fsys>. Use rustdoc for "what does this method do"; use the docs here for "how do these pieces fit together" and "which method should I pick."

## Map

- [`API.md`](API.md) &mdash; complete public-API surface, stable across `1.x` per [`STABILITY-1.0.md`](STABILITY-1.0.md). The initial freeze happened at `0.9.0`; backward-compatible additions through `0.9.x` are catalogued in [API additions in 0.9.1–0.9.x](API.md#api-additions-in-091097), all of which carry into `1.0` unchanged.
- [`EXAMPLES.md`](EXAMPLES.md) &mdash; catalogue of the 17 runnable examples in [`../examples/`](../examples/), each with a "when to use this pattern" guide. Run any example with `cargo run --example NN_name`.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) &mdash; internal layering, concurrency model, the native io_uring async substrate vs. `spawn_blocking` fallback selection, the 0.9.3 N-shard batch dispatcher, and per-handle resource lifecycle.
- [`METHODS.md`](METHODS.md) &mdash; durability-method matrix and how to choose between `Sync` / `Data` / `Direct` / `Mmap` / `Auto`. Includes the `Auto` decision ladder and the 0.9.x probe-input evolution.
- [`CRASH-SAFETY.md`](CRASH-SAFETY.md) &mdash; durability guarantees per method per platform, the 100&times; pre-merge crash-test protocol, the `0.9.0` journal subprocess-kill harness, and the journal tail-truncation taxonomy.
- [`PERFORMANCE.md`](PERFORMANCE.md) &mdash; tuning knobs (group-lane dispatcher, buffer pool, dispatcher shards, SQPOLL) and the `Builder::tune_for(Workload)` presets.
- [`PLATFORM-NOTES.md`](PLATFORM-NOTES.md) &mdash; per-OS quirks and capability requirements (NVMe passthrough, io_uring elite flags, ReFS/APFS reflinks, sparse-file primitives, OS-version probes).
- [`BENCH.md`](BENCH.md) &mdash; benchmark methodology, the `0.7.0` regression-budget infrastructure, the `0.8.0` peer comparison vs. `std::fs` and `tokio::fs`, the `0.9.0` journal-vs-atomic-replace measurement, and the 0.9.1&ndash;0.9.7 features awaiting bare-metal Linux re-runs.

## Status

**`1.0.0` is the first stable release.** The `1.x` line is API-stable and on-disk-format-stable per the contract in [`STABILITY-1.0.md`](STABILITY-1.0.md). The surface was frozen at the `0.9.0` release-candidate (2026-05-05); the `0.9.x` series added net-new public items backward-compatibly without breaking the freeze, and the `0.9.8` polish release finalised the documentation + examples + benchmarks. `1.0.0` carries the `0.9.x` shape forward verbatim under SemVer guarantees.

| Release | Phase |
|---|---|
| `0.9.0` RC | Shipped 2026-05-05 — initial API freeze |
| `0.9.1` – `0.9.7` | Shipped — backward-compatible additions: performance, observability, platform expansion |
| `0.9.8` | Shipped — polish, examples, benchmarks, `STABILITY-1.0.md` commitment doc |
| **`1.0.0`** | **Shipped — first stable release. SemVer + on-disk-format guarantees apply for the `1.x` line.** |

Per-version deltas live in [`../CHANGELOG.md`](../CHANGELOG.md).
