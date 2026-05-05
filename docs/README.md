<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  DOCUMENTATION
</h1>

The `docs/` tree is the long-form companion to the rustdoc API reference at <https://docs.rs/fsys>. Use rustdoc for "what does this method do"; use the docs here for "how do these pieces fit together" and "which method should I pick."

## Map

- [`API.md`](API.md) &mdash; complete public-API surface as of `0.9.0`, the three-tier entry points (`quick::*` / `builder().build()` / `Builder` chain), the `0.9.0` RC freeze policy, the journal substrate (`JournalHandle`, `JournalReader`, `JournalOptions`), and the rename matrix carried forward from `0.7.0`.
- [`EXAMPLES.md`](EXAMPLES.md) &mdash; catalogue of the 16 runnable examples in [`../examples/`](../examples/), each with a "when to use this pattern" guide. Run any example with `cargo run --example NN_name`.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) &mdash; internal layering, concurrency model, the `0.7.0` native io_uring async substrate vs. `spawn_blocking` fallback selection, and per-handle resource lifecycle.
- [`METHODS.md`](METHODS.md) &mdash; durability-method matrix and how to choose between `Sync` / `Data` / `Direct` / `Mmap` / `Auto`.
- [`CRASH-SAFETY.md`](CRASH-SAFETY.md) &mdash; durability guarantees per method per platform, the 100&times; pre-merge crash-test protocol, and the `0.9.0` journal subprocess-kill harness.
- [`PERFORMANCE.md`](PERFORMANCE.md) &mdash; tuning the group-lane dispatcher and the per-handle aligned buffer pool.
- [`PLATFORM-NOTES.md`](PLATFORM-NOTES.md) &mdash; per-OS quirks and capability requirements (NVMe passthrough, io_uring, sector-size discovery, etc.).
- [`BENCH.md`](BENCH.md) &mdash; benchmark methodology, the `0.7.0` regression-budget infrastructure, the `0.8.0` peer comparison vs. `std::fs` and `tokio::fs`, and the `0.9.0` journal-vs-atomic-replace measurement.

## Status

`0.9.0` is the **release candidate for 1.0** (2026-05-05). The public API documented in [`API.md`](API.md) is the 1.0 target shape; only genuine bugs may change a name or signature from this tag forward. Journal substrate (R-1 → R-2 → R-3) ships with three throughput tiers, a Direct-IO opt-in, sector-aligned log buffer, and crash-tested durability claims. 1.0 stable tags after the soak certification + bench peer-comparison + third-party crash-safety reproduction all clear.
