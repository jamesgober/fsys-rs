# Changelog

All notable changes to `fsys` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-05-04

### Added

- `Handle::write_batch<P: AsRef<Path>>(&self, batch: &[(P, &[u8])])`,
  `Handle::delete_batch<P: AsRef<Path>>(&self, batch: &[P])`,
  `Handle::copy_batch<P: AsRef<Path>, Q: AsRef<Path>>(&self, batch:
  &[(P, Q)])`, and `Handle::batch(&self) -> Batch<'_>` — the public
  group-lane batch API. Routes through a per-handle dispatcher
  thread that is spawned lazily on first batch op and shut down
  cleanly on `Handle` drop. Idle handles cost zero threads.
- `Batch<'_>` (re-exported from the crate root): chainable builder
  for very large or dynamic batches. `write` / `delete` / `copy`
  return `&mut Self`; `commit(self) -> Result<(), BatchError>`
  resolves paths against the handle root and submits.
- `BatchError` (re-exported from the crate root): per-batch failure
  type with `failed_at`, `completed`, and `source: Box<Error>`.
  Decision #5 semantics — independent ops, no rollback, stop on
  first failure (`Err` or panic).
- `Builder::batch_window_ms(u64)`, `Builder::batch_size_max(usize)`,
  and `Builder::batch_queue_max(usize)` — three new chainable
  knobs. Defaults: 1 ms / 128 ops / 1024-deep queue.
- `Error::ShutdownInProgress` (FS-00009) and reserved
  `Error::QueueFull` (FS-00010, never emitted in 0.4.0).
- Internal `pipeline` subsystem (crate-private): bounded MPMC
  queue + dispatcher thread + atomic-replace helper. Built on
  `crossbeam-channel`. See `.dev/DECISIONS-0.4.0.md` for the
  architecture record.
- 8 integration test files in `tests/`: `pipeline_basic`,
  `pipeline_concurrency` (16-thread stress), `pipeline_backpressure`
  (capacity-1 queue, blocking-not-error), `pipeline_shutdown`
  (drop-during-flight), `pipeline_panic` (post-failure recovery),
  `batch_builder`, `batch_ordering`, `batch_partial_failure`.
- 3 group-lane benchmarks in `benches/`: `batch_throughput` (size
  sweep 1/16/128/1024), `solo_vs_batch` (routing-decision
  verification), `concurrent_batches` (16-thread shared-handle
  contention).
- 3 panic-safety unit tests in `pipeline::group::tests` exercising
  the `catch_unwind` wrapper via a generic-executor variant of
  `process_jobs` (no test-only code in production paths — see
  decision D-6 in `.dev/DECISIONS-0.4.0.md`).

### Changed

- New dependency: `crossbeam-channel = "0.5"`. Justified inline in
  `Cargo.toml`: bounded MPMC + oneshot channels + `select!` macro.
  Established, near-universal in production Rust concurrent code,
  zero transitive deps. `std::sync::mpsc` is insufficient
  (no bounded MPMC, no usable `select`).
- `Handle::active_method()` reflects solo-lane state only in 0.4.0.
  Group-lane per-op Direct IO fallbacks are observable in
  `BatchError::source` for the failing op rather than aggregated
  to the handle. Cross-lane consistency lifts in 0.5.0 when the
  platform module's IO state machine is consolidated. See
  decision D-5 in `.dev/DECISIONS-0.4.0.md`.
- `Handle::new_raw` (`pub(crate)`) signature extended with a
  `pipeline: Pipeline` parameter. Internal-only; no public-API
  break.

## [0.3.0] - 2026-05-04
### Added
- `builder` and `handle` modules as the primary construction surface
  (`new()`, `with(method)`, `builder()`, `Builder`, `Handle`).
- `method` module with adaptive strategy selection and fallback-aware behavior.
- `crud` module for file and directory operations with root-scoped path safety.
- `quick` module for convenience helpers backed by a lazily initialized default
  handle.
- `meta` module for file metadata and permissions inspection.
- Cross-platform platform implementations for Linux, macOS, Windows, and
  unknown targets with best-effort fallback semantics.
- Direct IO paths with sector-aligned writes and Windows-aware non-buffered IO
  handling.

### Changed
- Crate version bumped to `0.3.0`.
- Public crate surface expanded in `lib.rs` with re-exports for primary
  construction and CRUD workflows.
- Error model expanded with additional operational and atomic-replace failure
  cases.

### Fixed
- Direct IO write path now truncates sector-padded temporary files back to the
  logical payload size before atomic rename.
- Windows direct-open test path now uses direct-write logic when a direct
  handle is returned.

## [0.2.0] - 2026-05-04
### Added
- `error` module: `Error` enum (`Io`, `InvalidPath`, `HardwareProbeFailed`,
  `UnsupportedPlatform`) marked `#[non_exhaustive]`, `Result<T>` type alias,
  stable `FS-XXXXX` error codes (FS-00001 through FS-00004), `Display` and
  `std::error::Error` implementations, and `From<std::io::Error>`.
- `os` module: `OsInfo`, `OsFamily`, `OsKind`, `Arch`, `Endianness`,
  cached `os::info()`, plus `os::name()`, `os::is_linux()`,
  `os::is_macos()`, `os::is_windows()`. Linux distro and kernel-release
  detection via `/etc/os-release` and `/proc/sys/kernel/osrelease`.
- `hardware` module: `HardwareInfo`, `DriveInfo`, `DriveKind`,
  `MemoryInfo`, `CpuInfo`, `CpuFeatures`, `IoPrimitives`, with
  cached accessors `hardware::info()`, `drive()`, `cpu()`,
  `io_primitives()`, plus live `hardware::memory()`. Logical-core
  count and compile-time CPU features are real; drive identification,
  capacity probing, physical-core enumeration, cache sizes, and
  memory probing are foundation-layer stubs (deferred to `0.0.5`,
  marked with `TODO(0.0.5)`).
- `path` module: `PathSet`, `Mode` enum (Dev / Prod / Auto with
  env-driven resolution from `FSYS_MODE` then `RUST_ENV`), per-OS
  prod defaults and CWD-relative dev defaults, plus `path::data()`,
  `bin()`, `config()`, `logs()`, `cache()`, `libs()`, `runtime()`,
  `temp()`, `state()`, `locks()` and matching `_for` accessors.
  `path::normalize()` and `path::sanitize_segment()` for separator
  normalisation and platform-safe segment sanitisation.
- Integration test `tests/foundation.rs` covering the public surface
  of the foundation layer.

### Changed
- Crate version bumped to `0.2.0` to match the foundation phase
  declared in `.dev/PLANNING.md`.

## [0.1.0] - 2026-05-04

### Added
- Initial release. Reserved name on crates.io. No public API.

[Unreleased]: https://github.com/jamesgober/fsys-rs/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/jamesgober/fsys-rs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jamesgober/fsys-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/fsys-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/fsys-rs/releases/tag/v0.1.0
