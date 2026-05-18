<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  STABILITY @ 1.0
</h1>

The `1.0.0` release commits the public API to **SemVer-stable**. This document is the contract. It specifies what 1.x guarantees, what 1.x explicitly does **not** guarantee, the MSRV policy, the deprecation policy, the breaking-change policy for 1.x patch and minor releases, and the relationship between `#[non_exhaustive]` types and forward compatibility.

This document supersedes the various per-version stability notes scattered across [`API.md`](API.md), `lib.rs`, and the release-note frontmatter. Read it once; it's authoritative.

---

## What 1.x guarantees

Every public item in the `fsys` crate at the `1.0.0` tag is part of the stable contract. Specifically:

### 1.1 The public API surface

The following are stable across the entire `1.x` line. None of these can be removed or have their signature changed (except by the exceptions listed in [§3](#3-what-1x-does-not-guarantee)) without a `2.0.0` major-version bump:

- **Re-exports from the crate root** (`pub use crate::...::*` in `lib.rs`): `Handle`, `Builder`, `Method`, `Mode`, `Workload`, `Batch`, `Lsn`, `JournalHandle`, `JournalOptions`, `JournalReader`, `JournalRecord`, `JournalTailState`, `SyncMode`, `WriteLifetimeHint`, `AsyncSubstrate`, `Advice`, `Error`, `Result`, `BatchError`, `DirEntry`, `FileMeta`, `Permissions`, the free functions `new()` / `with(method)` / `builder()`, the constant `VERSION`.
- **The `fsys::quick` module's free functions**: `quick::read`, `quick::write`, `quick::write_with`, `quick::delete`, `quick::exists`, `quick::size`, and their `_async` siblings (the latter behind the `async` feature).
- **The `fsys::observer` module**: the `FsysObserver` trait + its `JournalAppendEvent` / `JournalSyncEvent` / `HandleWriteEvent` / `HandleReadEvent` event-payload types.
- **The `fsys::hardware` / `fsys::os` / `fsys::path` accessor modules**: their `info()` snapshots and per-field accessors.
- **The `fsys::primitive` constants**: every `pub const &'static str` for durability primitive names.
- **The `fsys::async_io` module** (behind the `async` feature): every `_async` sibling of every sync method.

### 1.2 Method signatures

- Function and method **names** are stable.
- Function and method **parameter types** are stable. New optional parameters can only be added in a **major-version** bump (no defaulted-parameter equivalent in Rust).
- Function and method **return types** are stable. A method that returned `Result<T>` always returns `Result<T>` (the inner `T` is stable; the `Error` variant set can grow under the `#[non_exhaustive]` policy in [§1.3](#13-non_exhaustive-types)).
- **Generic bounds** on public items are stable. Loosening a bound is additive (allowed in minor releases); tightening a bound is breaking (requires major).
- **Trait method signatures** are stable. Adding a method to a public trait is breaking UNLESS the method has a default implementation that is forward-compatible (e.g., the existing `FsysObserver` default-noop pattern).

### 1.3 `#[non_exhaustive]` types

The following types are marked `#[non_exhaustive]` and may gain new variants / fields in minor releases. Match arms must include a `_` fallback; struct construction must use the type's constructor methods rather than struct literal syntax. The `#[non_exhaustive]` markers are themselves stable — they will not be removed in 1.x.

- `Method` (durability strategy enum)
- `Workload` (tuning preset enum)
- `JournalTailState` (journal recovery state enum)
- `SyncMode` (journal sync strategy enum)
- `WriteLifetimeHint` (Linux multi-stream NVMe hint enum)
- `AsyncSubstrate` (async substrate observability enum)
- `Advice` (page-cache hint enum)
- `Error` (the crate's error enum)
- `JournalAppendEvent`, `JournalSyncEvent`, `HandleWriteEvent`, `HandleReadEvent` (observer event payloads — `pub struct` rather than enum, but `#[non_exhaustive]` reserves the right to add fields)
- `DirEntry`, `FileMeta`, `Permissions` (metadata structs)
- `BatchError` (per-op failure container — fields private since 0.9.6; accessors stable)
- `Lsn` (since 0.9.6 — single-field newtype wrapping a `u64`; constructors stable)

### 1.4 Error variants

The `Error` enum is `#[non_exhaustive]`. New variants may be added in minor releases. Existing variants will not be removed or have their associated data shapes changed.

The `FS-XXXXX` **error codes** returned by `Error::code()` are **stable** at the assignment level — once a code is assigned to a variant, that mapping does not change. Code-grep contracts are safe.

The `Display` formatting of error variants is **not** part of the stable surface (see [§3.2](#32-display-formatting)). Match on `Error::code()` or pattern-match on the enum, not on the display string.

### 1.5 Crash safety, durability, atomicity

- The **atomic-replace contract** of `Handle::write`, `Handle::write_copy`, `Handle::write_batch`, and `Batch::commit`: the target file is either entirely the old payload (or absent) or entirely the new payload at every observable point. Never torn.
- The **journal durability contract** of `JournalHandle::sync_through`: after a successful return, every byte from offset 0 through `lsn.0 - 1` is on stable storage.
- The **journal tail-truncation taxonomy** of `JournalReader`: the 5 enumerated `JournalTailState` outcomes (`CleanEnd`, `TruncatedHeader`, `TruncatedPayload`, `ChecksumMismatch`, `BadMagic`, `LengthOverflow`) describe every possible reader outcome. Decode behavior for each is stable.
- The **CRC-32C frame format** of journal records: 12-byte overhead (4-byte magic+version + 4-byte length + 4-byte CRC-32C Castagnoli), little-endian length + CRC, 256 MiB max payload. Frames written by 1.x are readable by future 1.x versions.

### 1.6 Cargo features

The following cargo features are stable in `1.x`. Their semantics will not change; they will not be removed.

| Feature | Status | Semantics |
|---|---|---|
| (default — no features) | Stable | Sync API + journal + batch + cross-platform durability. |
| `async` | Stable | `_async` siblings via tokio; native io_uring substrate on Linux + Direct. |
| `tracing` | Stable | Structured spans + events on the write / read / journal hot paths. |

The following features are **internal** and not part of the stable surface. They may change or be removed at any time without a major-version bump:

| Feature | Status | Why internal |
|---|---|---|
| `stress` | Internal | Toggles soak-test duration; only affects test binaries. |
| `fuzz` | Internal | Exposes `__fuzz` helpers for the `fuzz/` workspace. |
| `oom_inject` | Internal | Replaces the global allocator for OOM-injection tests. Documented "NEVER enable in production builds." |

---

## 2. Versioning policy

`fsys` follows SemVer with the following clarifications:

### 2.1 `1.0.0` and forward

- **`1.x.y` patch releases**: backward-compatible bug fixes. No public-API changes. No MSRV bump.
- **`1.x.0` minor releases**: backward-compatible additions. New `pub fn`, new `pub struct`, new `pub enum` variants (under `#[non_exhaustive]`), new methods on existing types, new optional Cargo features. May bump MSRV (see [§4](#4-msrv-policy)).
- **`2.0.0` major release**: any change that breaks any of the guarantees in [§1](#1-what-1x-guarantees). Will be announced with a migration guide.

### 2.2 Pre-1.0 history (`0.x.y`)

Per Cargo's SemVer interpretation, `0.x.y` releases were allowed to break the API at every `0.x` bump. However, **`fsys` practiced API stability from the `0.9.0` release-candidate onward**: every release from `0.9.0` through `0.9.8` was backward-compatible with the previous one. The two pre-1.0 lockdowns (`Lsn` and `BatchError` field privatisation at 0.9.6) replaced public fields with stable accessor methods — the last shape changes before the `1.0` freeze.

`1.0.0` carries the `0.9.x` surface forward verbatim — no breaking changes between `0.9.8` and `1.0.0`.

### 2.3 Deprecation policy

Items deprecated within `1.x` are removed only in `2.0.0`:

1. Deprecation lands in a `1.x.0` minor release with `#[deprecated(since = "1.x.0", note = "use ... instead")]`.
2. The item continues to function unchanged in every subsequent `1.x.y` release.
3. Removal lands in `2.0.0`, no earlier than **6 months** after the deprecation was announced. A migration guide accompanies the `2.0.0` release.

Items may not be deprecated and removed in the same release.

---

## 3. What 1.x does **not** guarantee

These are explicitly **not** part of the stable contract. Callers depending on these properties accept that they may change between any two `1.x` releases.

### 3.1 `pub(crate)` internals

Anything not reachable through a public path from `lib.rs` is internal. The crate's internal modules (`crate::pipeline`, `crate::platform`, `crate::buffer`, `crate::method::auto`, `crate::method::mmap`, `crate::async_io::completion_driver`, `crate::async_io::iouring_substrate`, `crate::async_io::quick`, `crate::async_io::crud_dir`, `crate::async_io::crud_file`, `crate::async_io::batch`, `crate::async_io::journal`, `crate::hardware::probe`, etc.) have no stability promises. Their layout, function signatures, and internal types may change at any `1.x.y` patch boundary.

### 3.2 Display formatting

`Display` / `Debug` output of public types (`Error::Display`, `Method::Display`, `Lsn::Display`, etc.) is **not** stable. The exact wording, punctuation, and structure of these strings may be refined for clarity in any release. Tools that parse display output are broken by construction; use `Error::code()` for stable string contracts.

### 3.3 Default values

Default values of `Builder` knobs (`buffer_pool_count = 64`, `buffer_pool_block_size = 4096`, `io_uring_queue_depth = 128`, `batch_window_ms = 1`, `batch_size_max = 128`, `batch_queue_max = 1024`, `dispatcher_shards = 1`) are tuning constants, **not API contracts**. They may shift between releases as new workload data informs tuning. Programs that depend on specific defaults should set them explicitly.

The same applies to `JournalOptions` defaults (`log_buffer_kib = 64`, `group_commit_window = Some(500 µs)`, `group_commit_max_batch = 8`) and to the active-method selection by `Method::Auto` (the decision ladder in [`METHODS.md`](METHODS.md) may evolve as new hardware classes are surfaced by the probe).

### 3.4 Observable timings and resource usage

- Per-op latencies: not stable. Releases may include perf optimisations that change the latency profile of any operation.
- Per-handle memory footprint: not stable. The pool / queue / ring memory residency depends on lazy-allocation behavior that may evolve.
- Thread counts: idle handles cost zero threads (the dispatcher / completion driver are lazy); active handles' thread counts may vary as the pipeline architecture evolves.

### 3.5 Internal log and trace output

The `tracing` feature emits spans + events on the hot paths. The specific span names, field names, and event payloads are **not** stable. Production callers using `tracing` should match on the high-level span name (e.g., `"fsys::journal::append"`) rather than on its full field payload.

### 3.6 Platform-specific failure modes

Cross-platform IO has irreducible platform-specific failure surfaces. The set of `io::ErrorKind` values that can wrap inside `Error::Io` is essentially the union of every supported platform's possible IO errors. New `ErrorKind` values may surface in `1.x` if a new platform is added or an existing platform changes its return codes; the wrapping shape (`Error::Io(io::Error)`) is stable.

### 3.7 Performance characteristics under unusual hardware

The `Method::Auto` resolution ladder is calibrated against typical NVMe / SSD / HDD storage on the three supported platforms. Unusual setups (FUSE mounts, network filesystems, specialised flash classes, RAID volumes with mixed-class members) may resolve to a method that is technically correct but not the fastest available. `Auto`'s correctness is part of the stable contract; its optimality on unusual hardware is not.

---

## 4. MSRV policy

The Minimum Supported Rust Version (MSRV) is declared in `Cargo.toml` (`rust-version = "1.75"`).

- **`1.x.0` minor releases may bump MSRV** within the 12 most recent stable Rust versions at release time. If 1.x.0 ships during Rust 1.95's lifetime, MSRV may not exceed 1.83 (1.95 − 12).
- **`1.x.y` patch releases never bump MSRV**.
- **MSRV bumps require a corresponding minor-version bump** even when no other public-API change accompanies the MSRV move.

The `1.0.0` MSRV is `1.75`. Subsequent 1.x bumps will conform to the policy above.

---

## 5. Platform support

`fsys` supports three primary platforms in the stable surface:

| Platform | Stable in 1.x? | Notes |
|---|---|---|
| Linux (x86_64, aarch64) | ✓ | Primary perf target. io_uring + NVMe passthrough fast paths. |
| macOS (x86_64, aarch64) | ✓ | `F_FULLFSYNC` durability primitive. `F_BARRIERFSYNC` opt-in. |
| Windows (x86_64) | ✓ | `FlushFileBuffers` + `FILE_FLAG_WRITE_THROUGH`. NVMe IOCTL opt-in. |

Other platforms (FreeBSD, illumos, mobile targets, WASI, etc.) compile via the "unknown platform" fallback module but are **not** part of the stable contract. Production deployments on non-tier-1 platforms should verify via the test suite + the [`docs/PLATFORM-NOTES.md`](PLATFORM-NOTES.md) caveats list.

The `#[cfg(...)]` gates around platform-specific code are an implementation detail and may shift between releases; the **observable behavior** of each public method on each tier-1 platform is stable.

---

## 6. Migration into `1.0`

Callers already on `0.9.x` need no code changes for `1.0`. The two pre-1.0 lockdowns (`Lsn` and `BatchError` field privatisation) shipped at `0.9.6` and have been the public surface ever since.

Specifically:

- **`0.9.0` → `0.9.x` → `1.0.0`**: zero breaking changes. The 0.9.0 RC freeze held through every subsequent release.
- **`0.8.x` → `1.0.0`**: the 0.7.0 rename audit (read_range → read_at, etc.) and the `Lsn` + `BatchError` field privatisation are the only breaking changes. Programs that compile against `0.9.0` will compile against `1.0.0` unchanged.
- **`0.7.x` → `1.0.0`**: apply the 0.7.0 rename audit's method-name updates (covered in [`API.md`](API.md#api-changes-in-070-carried-forward)) and the Lsn / BatchError lockdowns.

---

## 7. Process for proposing changes that break this contract

If a real-world bug or design defect makes a 1.x-stable item unsafe or incorrect to use as documented:

1. **Open a GitHub issue** explaining the defect with reproduction steps.
2. **Discuss the fix shape**: in-place fix (preferred — patches the bug while preserving the signature), additive workaround (new method that does the right thing; deprecate the broken one), or breaking change (requires 2.0).
3. **No breaking change is shipped in a 1.x.y patch.** Bugs that genuinely require breaking changes are scheduled for the next major.

The maintainer ([James Gober](https://github.com/jamesgober)) holds the final call on whether a proposed fix is breaking or non-breaking under this contract.

---

## See also

- [`API.md`](API.md) — full public-API reference for the surface this document commits to.
- [`CHANGELOG.md`](../CHANGELOG.md) — per-version delta history.
- [`PLATFORM-NOTES.md`](PLATFORM-NOTES.md) — platform-specific behavior, capability requirements, and fallback ladders.
- [`METHODS.md`](METHODS.md) — `Method` taxonomy + `Auto` decision ladder.
- [`CRASH-SAFETY.md`](CRASH-SAFETY.md) — per-method durability contract.
