# Changelog

All notable changes to `fsys` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/jamesgober/fsys-rs/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/jamesgober/fsys-rs/releases/tag/v0.3.0
[0.2.0]: https://github.com/jamesgober/fsys-rs/releases/tag/v0.2.0
