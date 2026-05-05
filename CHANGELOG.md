# Changelog

All notable changes to `fsys` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-05-04

### Added

- **Async layer (gated behind the `async` Cargo feature).** Every
  sync method on [`Handle`] gets an `_async` sibling: `write_async`,
  `read_async`, `write_at_async`, `write_copy_async`,
  `append_async`, `delete_async`, `truncate_async`, `rename_async`,
  `copy_async`, `read_range_async`, `exists_async`, `size_async`,
  `meta_async`, `mkdir_async`, `mkdir_all_async`, `rmdir_async`,
  `rmdir_all_async`, `list_async`, `scan_async`, `find_async`,
  `count_async`, `is_dir_async`, `is_file_async`. Plus async batch:
  `write_batch_async`, `delete_batch_async`, `copy_batch_async`. And
  async `quick`: `fsys::async_io::quick::{write_async, read_async,
  delete_async, write_with_async}`.
  - Single-op CRUD wrappers route through
    [`tokio::task::spawn_blocking`] (locked decision D-1).
  - Async batch routes through the existing per-handle dispatcher
    via `tokio::sync::oneshot` (locked decision D-5). The
    dispatcher's job carries a new `BatchResponse` enum:
    `Sync(crossbeam_channel::Sender<...>)` or
    `Async(tokio::sync::oneshot::Sender<...>)`. The dispatcher
    matches exhaustively on the variant.
  - 18 `#[tokio::test]` integration tests covering the full async
    surface; 100× pre-merge stability run on the batch path
    confirmed the enum refactor is regression-free.

- **NVMe passthrough flush on Linux and Windows** (locked decision
  D-2).
  - **Linux:** `NVME_IOCTL_IO_CMD` ioctl carrying NVMe FLUSH
    (opcode 0x00). Capability detection at the first Direct op:
    resolves the file's underlying block device, opens
    `/dev/nvmeX` with `O_RDWR`, caches success/failure on the
    Handle. Falls back to `fdatasync` when not capable.
  - **Windows:** `IOCTL_STORAGE_PROTOCOL_COMMAND` with
    `ProtocolTypeNvme` carrying NVMe FLUSH. Capability detection
    via Identify Controller probe; falls back to
    `FILE_FLAG_WRITE_THROUGH` when not capable.
  - **macOS:** intentionally not supported (Apple does not expose
    the necessary primitives in mainstream APIs). `Method::Direct`
    on macOS continues to use `F_NOCACHE + F_FULLFSYNC`.
  - `FSYS_DISABLE_NVME_PASSTHROUGH=1` environment variable forces
    the fallback path. Testing aid only — production callers who
    want to disable passthrough should explicitly pick
    `Method::Data` or `Method::Sync`.

- **`Handle::active_durability_primitive() -> &'static str`** —
  new accessor returning the canonical name of the durability
  primitive currently in use (e.g. `"io_uring + NVMe FLUSH"`,
  `"FILE_FLAG_WRITE_THROUGH"`, `"F_FULLFSYNC"`). Match against the
  public constants in the new [`fsys::primitive`] module to avoid
  string-typo bugs.

- **`fsys::primitive`** — new public module of canonical
  durability-primitive strings. Stable across `0.x.y` releases.

- **Completion CRUD methods.**
  - `Handle::write_copy(path, &data)` — atomic-swap with
    metadata preservation. Unix: mode unconditional, owner/group
    silent-skip-on-EPERM, mtime/atime via `utimensat`. Windows:
    timestamps via `SetFileTime`, ACLs via
    `GetNamedSecurityInfoW` / `SetNamedSecurityInfoW`.
  - `Handle::scan(path, recursive)` — directory walk, optionally
    recursive. Symlinks not followed in 0.6.0 (F-14 for 0.7.0+).
  - `Handle::find(path, pattern)` — glob-based search. Standard
    `glob` crate syntax (`*`, `**`, `?`, `[abc]`, `[!abc]`) plus
    brace alternation `{foo,bar}` via a custom expansion
    preprocessor (the `glob` crate doesn't natively support
    braces).
  - `Handle::count(path, recursive)` — count regular files at or
    under `path`.
  - `Handle::truncate(path, new_size)` — resize a file.
  - `Handle::rename(old, new)` — atomic rename / move.

- **4 new error variants.**
  - `Error::NvmePassthroughUnsupported` (FS-00015).
  - `Error::NvmePassthroughDenied` (FS-00016).
  - `Error::AsyncRuntimeRequired` (FS-00017) — returned by `_async`
    methods when called outside a tokio runtime.
  - `Error::GlobPatternInvalid { reason }` (FS-00018).

- **Stress / soak / fuzz infrastructure (pragmatic mode per
  locked decision D-7).**
  - `tests/stress.rs` — 3 soak tests gated behind the new
    `stress` Cargo feature: 60 s default budget, 1 hour with
    `--features stress`. Validates no memory growth, no thread
    leaks, no per-handle resource leaks under continuous mixed
    CRUD load.
  - `tests/edge_cases.rs` — 11 edge-case tests covering 0-byte
    payloads, exact page/sector boundaries, Unicode + emoji
    paths, deeply nested directories, `MAX_PATH`-safe long
    filenames, atomic-rename racing.
  - `fuzz/` — cargo-fuzz workspace with three targets:
    `path_normalize`, `glob_pattern`, `batch_builder`. Per
    pragmatic mode, dev iteration runs ~60 s / 100K iterations;
    CI nightly and pre-release runs the documented 1 hour / 1M
    iterations.

- **`docs/` directory** at the repo root with six user-facing
  documents: `ARCHITECTURE.md`, `METHODS.md`, `PERFORMANCE.md`,
  `CRASH-SAFETY.md`, `PLATFORM-NOTES.md`, `MIGRATION.md`.

### Changed

- **New runtime dependency:** `glob = "0.3"` (always-on, required
  by `Handle::find`). Justified inline in `Cargo.toml`: mature
  (~300k weekly downloads), well-maintained, no transitive
  bloat. Selected over rolling our own glob (~500 LOC).

- **Tokio feature set:** dropped `"fs"`, added `"rt"`,
  `"rt-multi-thread"`, `"sync"`, `"macros"`. The async layer uses
  `spawn_blocking` against the sync core, never tokio's `fs`
  primitives.

- **`windows-sys` features:** added
  `Win32_Security_Authorization` for the `write_copy` ACL
  preservation path.

- **`BatchJob.response`** changed from
  `crossbeam_channel::Sender<Result<…>>` to a new
  `BatchResponse` enum (Sync or Async). Internal change — public
  batch API surface is unchanged. The dispatcher matches
  exhaustively on the enum.

### Notes

- `Method::Direct` on Linux now has three execution paths:
  1. **io_uring + NVMe passthrough** (preferred when capable).
  2. **io_uring + fdatasync** (when the ring is available but
     NVMe passthrough isn't).
  3. **`O_DIRECT` + `pwrite` + `fdatasync`** (final fallback).
  `active_durability_primitive()` reports the actual primitive in
  use.

- `Method::Direct` on Windows now has two execution paths:
  1. **`FILE_FLAG_WRITE_THROUGH` + NVMe IOCTL** (when admin +
     capable hardware).
  2. **`FILE_FLAG_WRITE_THROUGH`** (fallback).

- Async layer adds zero new threads. `spawn_blocking` uses tokio's
  existing blocking pool; the batch async path shares the
  per-handle dispatcher with sync batches.

- `cargo-fuzz` is **not** required for routine `cargo build` /
  `cargo test`. The `fuzz/` directory is its own workspace; main
  workspace builds ignore it.

## [0.5.1] - 2026-05-04

### Added

- **Real `io_uring` integration on Linux.** `Method::Direct` writes
  and reads route through a per-handle io_uring ring (lazy-
  constructed on the first Direct op via the existing
  [`Builder::io_uring_queue_depth`] knob). Atomic-replace path
  submits `Write` + `Fsync(DATASYNC)` SQEs through the ring; reads
  use a single `Read` SQE.
- New module [`crate::platform::linux_iouring`] implementing the
  ring as an owner-thread design: the `io_uring::IoUring` value is
  owned by a dedicated thread, and submitters forward operations
  through a bounded `crossbeam_channel`. Caller blocks on a per-op
  reply channel, which keeps borrowed buffers alive across the
  syscall. New unit tests
  (`ring_construction_returns_ring_or_setup_failed`,
  `write_at_round_trip`, `read_at_round_trip`,
  `concurrent_submitters_serialise_through_owner`) validate the
  wrapper on every Linux CI run.

### Fixed

- `tests/foundation.rs::hardware_helpers_return_consistent_data`
  now accepts drift in `DriveInfo::available_bytes` between two
  consecutive live probes (free-disk movement on the runner caused
  CI flakes). Same accommodation that was already in place for
  `MemoryInfo::available_bytes` since 0.5.0.

### Notes

- `Method::Direct` on Linux now has two execution paths:
  1. **io_uring** (preferred) — used when `io_uring_setup(2)` and
     ring construction succeed.
  2. **`O_DIRECT` + `pwrite` + `fdatasync`** (fallback) — used when
     the ring is unavailable (kernel < 5.1, SECCOMP/AppArmor block,
     container restriction, runtime submit failure).
  Both paths satisfy the same atomic-replace + durability contract;
  `active_method()` is **not** downgraded for the io_uring fallback
  alone (this differs from the Mmap fallback per R-2''' in
  `.dev/DECISIONS-0.5.0.md`).
- Cached failure: once `IoUringRing::new` fails for a Handle, the
  ring slot transitions to `Disabled` and subsequent Direct ops
  skip the construction attempt — they go straight to the
  `pwrite`+`fdatasync` fallback.

### Internal

- The rustc 1.95 ICE that blocked the 0.5.0 lift was diagnosed as a
  panic in the `dead_code` (`check_mod_deathness`) analysis pass
  (`slice index starts at 23 but ends at 21`), specifically when
  the `linux_iouring` module's items are scanned. Module-level
  `#![allow(dead_code)]` skips the buggy lint path entirely without
  affecting correctness — every public item in the module is
  reachable from `Handle::io_uring_ring`. The owner-thread design
  is also preserved as the architectural choice for !Sync resources
  (it generalises to per-thread sharded rings in 0.6.0 without an
  API break).

## [0.5.0] - 2026-05-04

### Added

- **Real hardware probe** replacing the 0.2.0 stub. Per-platform
  implementations under `src/hardware/probe/`: Linux uses
  `/proc/self/mountinfo` → `/sys/dev/block/`, `/proc/meminfo`,
  `/proc/cpuinfo`; macOS uses `statvfs` + `sysctlbyname`; Windows
  uses `GlobalMemoryStatusEx`, `GetLogicalProcessorInformationEx`,
  `GetDiskFreeSpaceW`, and `IOCTL_STORAGE_QUERY_PROPERTY`.
  `DriveInfo`, `MemoryInfo`, `CpuInfo`, and `IoPrimitives` now
  return live values instead of constants.
- `PlpStatus` (`Yes` / `No` / `Unknown`) replaces `DriveInfo::plp:
  bool`. Tri-state surfaces the "we genuinely could not determine"
  case instead of silently coercing it to `false`. **Breaking
  change in 0.x** — see migration note below.
- `Method::Mmap` upgraded from reserved to a real implementation
  (`memmap2` + `msync` / `FlushViewOfFile` + atomic rename) with
  per-handle suitability fallback to `Method::Sync` for sub-page
  payloads, zero-length writes, and non-regular files (R-2'' in
  `.dev/DECISIONS-0.5.0.md`). Fallback is observable via
  `Handle::active_method()` and is permanent for the lifetime of
  the handle.
- `Method::Auto` is now genuinely hardware-aware. Resolution ladder
  (per-platform, drive-class indexed) lives in `src/method/auto.rs`
  and is locked in DECISIONS-0.5.0.md as decision #2.
- Per-handle aligned buffer pool. `crossbeam-queue::ArrayQueue` for
  the lock-free fast path with a `Mutex<()>` + `Condvar` slow path
  for waiters. Lazily allocated on first use; no cost for handles
  that never need aligned IO.
- `Builder` knobs: `buffer_pool_size(usize)`,
  `buffer_pool_block(usize)`, `io_uring_queue_depth(u32)`. Defaults
  64 / 4096 / 128.
- New error variants: `IoUringSetupFailed` (FS-00011),
  `MmapFailed` (FS-00012), reserved `BufferPoolExhausted`
  (FS-00013), `PlpDetectionUnavailable` (FS-00014).
- 4 crash-safety integration test binaries (`tests/crash_sync.rs`,
  `tests/crash_data.rs`, `tests/crash_direct.rs`,
  `tests/crash_mmap.rs`) sharing `tests/crash_harness/mod.rs`. Each
  binary covers `PreSyscall` / `MidSyscall` / `PostSyscall` kill
  modes via subprocess + stdout-line-based deterministic
  synchronisation (D-2). 100× pre-merge stability protocol from
  D-4 verified locally — 400/400 binary runs (1 200 individual
  test executions) green.
- 3 new benchmarks: `method_payload_matrix` (4 methods × 4
  payloads × 4 ops, the canonical 0.5.x regression surface),
  `mmap_workloads` (page-aligned fast path vs sub-page Sync
  fallback), `direct_iouring` (post-stub baseline for
  `Method::Direct`).
- `.dev/DECISIONS-0.5.0.md` — full architectural decision log for
  this release: 7 locked decisions (D-1..D-7), R-2 / R-2' / R-2''
  iteration notes for the Mmap suitability fallback, and the
  io_uring blocker section.

### Changed

- New runtime dependencies: `crossbeam-queue = "0.3"` (buffer pool;
  D-5), `memmap2 = "0.9"` (mmap implementation; D-6).
- `windows-sys` features extended:
  `Win32_System_SystemInformation`, `Win32_Storage_IscsiDisc`,
  `Win32_System_Ioctl`, `Win32_System_Pipes` for the hardware
  probe and crash-test harness.
- `Method::Mmap` and `Method::Auto` are no longer reserved — both
  are routable in 0.5.0.

### Notes

- **`io_uring` is stubbed in 0.5.0.** A rustc 1.95
  `check_mod_deathness` ICE fires when `io_uring::IoUring` is
  wrapped in any `std::sync` primitive; reproducible across
  io-uring 0.6.x and 0.7.x and across `Mutex` / `RwLock` /
  `UnsafeCell` wrappers. `Method::Direct` on Linux therefore
  currently runs the `O_DIRECT` + `pwrite` + `fdatasync` fallback
  path. The full lift checklist is in DECISIONS-0.5.0.md; the
  `direct_iouring` bench is the post-stub baseline that the 0.5.x
  patch will compare against.

### Migration (0.4 → 0.5)

- `DriveInfo::plp` changed type from `bool` to `PlpStatus`. Match
  on the enum: `PlpStatus::Yes` is the only state that previously
  read `true`; `PlpStatus::No` and `PlpStatus::Unknown` previously
  read `false`. Code that treats "Unknown" as "No" should use
  `matches!(info.plp, PlpStatus::Yes)`.

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
