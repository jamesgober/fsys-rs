<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  DOCUMENTATION
</h1>

The `docs/` tree is the long-form companion to the rustdoc API reference at <https://docs.rs/fsys>. Use rustdoc for "what does this method do"; use the docs here for "how do these pieces fit together" and "which method should I pick."

## Map

- [`API.md`](API.md) &mdash; complete public-API surface. The API was frozen at the `0.9.0` release-candidate; backward-compatible additions in `0.9.1`&ndash;`0.9.7` are catalogued in [API additions in 0.9.1&ndash;0.9.7](API.md#api-additions-in-091097).
- [`EXAMPLES.md`](EXAMPLES.md) &mdash; catalogue of the 17 runnable examples in [`../examples/`](../examples/), each with a "when to use this pattern" guide. Run any example with `cargo run --example NN_name`.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) &mdash; internal layering, concurrency model, the native io_uring async substrate vs. `spawn_blocking` fallback selection, the 0.9.3 N-shard batch dispatcher, and per-handle resource lifecycle.
- [`METHODS.md`](METHODS.md) &mdash; durability-method matrix and how to choose between `Sync` / `Data` / `Direct` / `Mmap` / `Auto`. Includes the `Auto` decision ladder and the 0.9.x probe-input evolution.
- [`CRASH-SAFETY.md`](CRASH-SAFETY.md) &mdash; durability guarantees per method per platform, the 100&times; pre-merge crash-test protocol, the `0.9.0` journal subprocess-kill harness, and the journal tail-truncation taxonomy.
- [`PERFORMANCE.md`](PERFORMANCE.md) &mdash; tuning knobs (group-lane dispatcher, buffer pool, dispatcher shards, SQPOLL) and the `Builder::tune_for(Workload)` presets.
- [`PLATFORM-NOTES.md`](PLATFORM-NOTES.md) &mdash; per-OS quirks and capability requirements (NVMe passthrough, io_uring elite flags, ReFS/APFS reflinks, sparse-file primitives, OS-version probes).
- [`BENCH.md`](BENCH.md) &mdash; benchmark methodology, the `0.7.0` regression-budget infrastructure, the `0.8.0` peer comparison vs. `std::fs` and `tokio::fs`, the `0.9.0` journal-vs-atomic-replace measurement, and the 0.9.1&ndash;0.9.7 features awaiting bare-metal Linux re-runs.

## Status

The `0.9.x` minor series is the **release-candidate phase for `1.0`**. The public API was frozen at the `0.9.0` RC tag (2026-05-05) and carries forward through every subsequent release backward-compatibly. Six minor releases (`0.9.1` through `0.9.7`) have shipped since the freeze, adding net-new public surface without breaking the existing one. The `0.9.8` release is the **final polish + 1.0-RC preparation** &mdash; documentation refresh, examples expansion, canonical Linux benchmarks, and the [`STABILITY-1.0.md`](STABILITY-1.0.md) commitment doc that will accompany the `1.0` stable tag.

| Phase | Status |
|---|---|
| `0.9.0` RC | Shipped 2026-05-05 — API frozen |
| `0.9.1`&ndash;`0.9.6` | Shipped — backward-compatible additions, performance / observability / platform expansion |
| `0.9.7` | Shipped 2026-05-12 — completion + optimisation + stabilisation (every 0.9.6 audit carryover resolved) |
| `0.9.8` | In progress — final polish, examples, benchmarks, stability commitment |
| `1.0.0` | After `0.9.8` ships + real-world certification clears |

Per-version deltas live in [`../CHANGELOG.md`](../CHANGELOG.md).
