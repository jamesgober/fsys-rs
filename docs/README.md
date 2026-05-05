<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  DOCUMENTATION
</h1>

The `docs/` tree is the long-form companion to the rustdoc API reference at <https://docs.rs/fsys>. Use rustdoc for "what does this method do"; use the docs here for "how do these pieces fit together" and "which method should I pick."

## Map

- [`API.md`](API.md) &mdash; complete public-API surface as of `0.7.0`, the three-tier entry points (`quick::*` / `builder().build()` / `Builder` chain), the `0.8.0` alpha-freeze policy, and the rename matrix for breaking changes in `0.7.0`.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) &mdash; internal layering, concurrency model, the `0.7.0` native io_uring async substrate vs. `spawn_blocking` fallback selection, and per-handle resource lifecycle.
- [`METHODS.md`](METHODS.md) &mdash; durability-method matrix and how to choose between `Sync` / `Data` / `Direct` / `Mmap` / `Auto`.
- [`CRASH-SAFETY.md`](CRASH-SAFETY.md) &mdash; durability guarantees per method per platform, and the 100&times; pre-merge crash-test protocol.
- [`PERFORMANCE.md`](PERFORMANCE.md) &mdash; tuning the group-lane dispatcher and the per-handle aligned buffer pool.
- [`PLATFORM-NOTES.md`](PLATFORM-NOTES.md) &mdash; per-OS quirks and capability requirements (NVMe passthrough, io_uring, sector-size discovery, etc.).
- [`BENCH.md`](BENCH.md) &mdash; benchmark methodology, the `0.7.0` regression-budget infrastructure, and the native-vs-`spawn_blocking` measurement protocol.

## Status

`0.7.0` shipped (2026-05-04). The public API is feature-complete for `1.0` and **frozen at the upcoming `0.8.0` alpha tag**. From `0.8.0` onward, only genuine bugs may change a name or signature.
