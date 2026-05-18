<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  DOCUMENTATION
</h1>

The `docs/` tree is the long-form companion to the rustdoc API reference at <https://docs.rs/fsys>. Use rustdoc for "what does this method do"; use the docs here for "how do these pieces fit together" and "which method should I pick."

## Map

- [`API.md`](API.md) &mdash; complete public-API surface, stable across `1.x` per [`STABILITY-1.0.md`](STABILITY-1.0.md). The initial freeze happened at `0.9.0`; backward-compatible additions through `0.9.x` and `1.1.0` (capability cache, journal backend observability, `Method::Spdk` gating) extend the surface without breaking it.
- [`EXAMPLES.md`](EXAMPLES.md) &mdash; catalogue of the 33 runnable examples in [`../examples/`](../examples/), each with a "when to use this pattern" guide. Run any example with `cargo run --example NN_name`.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) &mdash; internal layering, concurrency model, the native io_uring async substrate vs. `spawn_blocking` fallback selection, the 0.9.3 N-shard batch dispatcher, and per-handle resource lifecycle.
- [`METHODS.md`](METHODS.md) &mdash; durability-method matrix and how to choose between `Sync` / `Data` / `Direct` / `Mmap` / `Auto`. Includes the `Auto` decision ladder and the 0.9.x probe-input evolution.
- [`CRASH-SAFETY.md`](CRASH-SAFETY.md) &mdash; durability guarantees per method per platform, the 100&times; pre-merge crash-test protocol, the `0.9.0` journal subprocess-kill harness, and the journal tail-truncation taxonomy.
- [`PERFORMANCE.md`](PERFORMANCE.md) &mdash; tuning knobs (group-lane dispatcher, buffer pool, dispatcher shards, SQPOLL) and the `Builder::tune_for(Workload)` presets.
- [`PLATFORM-NOTES.md`](PLATFORM-NOTES.md) &mdash; per-OS quirks and capability requirements (NVMe passthrough, io_uring elite flags, ReFS/APFS reflinks, sparse-file primitives, OS-version probes).
- [`SPDK.md`](SPDK.md) *(1.1.0)* &mdash; hardware requirements, system setup, capability probe, configuration knobs, and per-`SpdkSkipReason` remediation steps for the SPDK kernel-bypass backend (Linux-only, gated by the `spdk` Cargo feature + the companion `fsys-spdk` crate).
- [`BENCH.md`](BENCH.md) &mdash; benchmark methodology, the `0.7.0` regression-budget infrastructure, the `0.8.0` peer comparison vs. `std::fs` and `tokio::fs`, the `0.9.0` journal-vs-atomic-replace measurement, the 1.1.0 capability-access + backend-accessor benches, and the 0.9.1&ndash;0.9.7 features awaiting bare-metal Linux re-runs.

## Status

**`1.1.0` is the current release.** The `1.x` line is API-stable and on-disk-format-stable per the contract in [`STABILITY-1.0.md`](STABILITY-1.0.md). `1.0.0` shipped the first frozen surface; `1.1.0` is a purely additive minor release that extends the public API with the capability cache, the journal backend observability accessors, the `Method::Spdk` runtime gate, and two new error variants. Every 1.0 public item works identically in 1.1.0.

| Release | Phase |
|---|---|
| `0.9.0` RC | Shipped 2026-05-05 — initial API freeze |
| `0.9.1` – `0.9.7` | Shipped — backward-compatible additions: performance, observability, platform expansion |
| `0.9.8` | Shipped — polish, examples, benchmarks, `STABILITY-1.0.md` commitment doc |
| `1.0.0` | Shipped 2026-05-14 — first stable release. SemVer + on-disk-format guarantees apply for the `1.x` line. |
| **`1.1.0`** | **Shipped 2026-05-18 — capability cache, SPDK eligibility surface, JournalBackend trait + observability accessors, `Method::Spdk` runtime gate. 100% additive vs. `1.0.0`.** |

Per-version deltas live in [`../CHANGELOG.md`](../CHANGELOG.md).
