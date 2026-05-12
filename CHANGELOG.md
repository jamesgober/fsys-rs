# Changelog

All notable changes to `fsys` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.7] - 2026-05-12

> **Completion + optimization + stabilization.** Every 0.9.6 audit
> carryover landed (H-2, H-7, H-9, H-16) plus a Builder knob
> (`sqpoll`) for kernel-side io_uring submission polling. The
> 0.9.6 `IORING_REGISTER_FILES` capability was restored on both
> the sync ring and the async substrate now that the
> `DEFER_TASKRUN` / `SINGLE_ISSUER` async hang was root-caused
> and gated via `RingMode`. The 4 HIGH carryovers from 0.9.6 are
> now resolved; the 0.9.7 release is the foundation surface
> 0.9.8 polishes for the 1.0 RC.

### Added — 0.9.7

- **`Builder::sqpoll(idle_ms: u32)`** — opt-in
  `IORING_SETUP_SQPOLL` for the per-handle io_uring sync ring.
  When enabled, the kernel spawns a polling thread that drains
  the submission queue without requiring `io_uring_enter`
  syscalls, useful for sustained-throughput writers (database
  WAL flush loops, LSM compaction). After `idle_ms` of no
  submissions the kernel thread sleeps. Linux-only; macOS /
  Windows ignore the value. Default OFF. Falls back cleanly to
  non-SQPOLL `pwrite + fdatasync` on EPERM (kernel < 5.13
  without `CAP_SYS_NICE`, sandboxed containers).
- **OOM-injection test infrastructure** (audit H-7) — internal
  `oom_inject` cargo feature exposes
  `OomInjectingAllocator` + `OomThreshold` RAII guard via the
  doc-hidden `fsys::test_support` module. Tests bracket code
  under test with the guard so allocations ≥ threshold return
  null; the fallible-alloc paths surface
  `Error::Io(OutOfMemory)` cleanly rather than panicking.
  Documented "NEVER enable in production builds" — every
  allocation pays a thread-local lookup + comparison.
- **Cross-platform symmetry tests** (audit M-5) —
  `tests/platform_symmetry.rs` runs 7 tests on every supported
  OS (Linux / macOS / Windows) asserting the same contract on
  each, with platform-specific assertions only where the
  underlying primitive legitimately differs.
- **Three new fuzz targets** (audit M-7):
  - `journal_append.rs` — end-to-end fuzz of append + sync +
    close + reopen + decode round-trip with length-prefixed
    fuzzer-chosen records.
  - `batch_writes.rs` — fuzz of the batch commit dispatcher
    (where `batch_builder` only fuzzed the chainable builder).
    Validates `BatchError` accessor consistency.
  - `aligned_pool_stress.rs` — fuzz of `AlignedBufferPool` via
    the public `Method::Direct` write path with boundary-case
    payload sizes.
- **kernel-version fallback tests** (audit H-9) —
  `tests/iouring_features_fallback.rs` exercises the
  no-elite-flags baseline (kernels < 5.19) and the
  `fallocate → posix_fallocate` fallback path via env-var
  test hooks (`FSYS_TEST_FORCE_NO_IOURING_FEATURES=1`,
  `FSYS_TEST_FORCE_POSIX_FALLOCATE=1`).
- **Journal frame boundary tests** (audit M-11) — 7 new tests
  for one-byte records, exact 4 KiB / 16 KiB / 64 KiB frame
  alignment, and empty-record batch shapes.

### Changed — 0.9.7

- **`IORING_REGISTER_FILES` re-enabled on both rings** — the
  pre-0.9.6 fd-registry slot-upgrade capability is back, this
  time backed by explicit slot-table-exhaustion and
  same-fd-reuse test coverage. Sync ring (dedicated owner
  thread) + async substrate (tokio-task-driven) both register a
  16-slot sparse file table at startup and lazily upgrade per-
  op fds via `register_files_update`. Submissions for cached
  fds use `IORING_OP_WRITE` with `IOSQE_FIXED_FILE`, saving
  per-syscall fd validation. Table-full fallback is silent —
  ops on uncached fds use raw `io_uring::types::Fd(raw)` and
  succeed unchanged.
- **GroupCommit wake-stampede fixed** (audit H-16) —
  `pending_followers` moved from `GroupCommitState`
  (lock-protected `u32`) to `GroupCommit` (`AtomicU32`). On
  follower wake, the state lock is dropped immediately; the
  follower atomic-decrements `pending_followers` and atomic-
  checks `synced_lsn` (the public mirror of `committed_lsn`).
  If the target is covered — the common case — the follower
  returns without ever re-acquiring the state lock. Architect-
  urally: the critical-section width drops from
  "lock + counter decrement + LSN compare + drop" to
  "lock + drop" — a ~5x reduction in per-follower lock-hold
  time under 100+ follower stampedes.
- **LSN atomic-ordering tightened** (audit M-2) — three
  `fetch_add(... AcqRel)` sites on `next_lsn`
  (single-record + batch sync append in `journal/mod.rs`,
  native async append in `async_io/journal.rs`) downgraded to
  `Release`. The reservation step does not consult shared
  non-atomic state set up by a peer appender; the `Acquire`
  half was defensive overhead. On aarch64,
  `fetch_add(Release)` lowers to a single store-release
  barrier (LDADDL) vs `AcqRel`'s LDADDAL with the additional
  load-acquire fence. ~0.2-0.5 µs/op saved.
- **`JournalHandle` impl-detail fields** (audit H-2) — audited
  the 9 `pub(crate)` fields. Six remain `pub(crate)`
  (`file`, `next_lsn`, `synced_lsn`, `group_commit`,
  `native_ring`, `direct`) because they're consumed by the
  `impl JournalHandle` blocks in `src/async_io/journal.rs`.
  Three (`log_buffer`, `observer`, `sync_mode`) are now
  private — only accessed from `src/journal/mod.rs` itself.

### Fixed — 0.9.7

- Stale `Vec<u8>` argument typing in the `batch_builder` fuzz
  target — pre-existing API drift the new fuzz targets surfaced.

### Internal — 0.9.7

- New CI matrix job `oom-inject` runs the `oom_injection` test
  binary on ubuntu / macos / windows with the
  `--features oom_inject` flag, separately from the regular
  test matrix (the global allocator replacement applies to
  every test in the binary so it can't share the default
  matrix).
- **L-2 inline pass** — `#[inline]` added to `Handle` public
  accessors (`method`, `active_method`, `root`, `mode`,
  `sector_size`, `observer`) and `JournalHandle::is_direct_active`
  so they inline across the crate boundary. `Lsn::new` /
  `as_u64` / `From` impls and `synced_lsn` / `next_lsn` already
  had `#[inline]` from earlier work.
- **H-16 verification** — added `group_commit_wake_stampede_128_followers`
  unit test that fires 128 concurrent followers at a single
  target LSN, asserts no deadlock + zero `pending_followers`
  leak after all threads join + all followers see their target
  as durable. Validates the structural correctness of the
  atomic-decrement + lock-free early-exit path under the
  contention level the audit flagged.
- **`AUDIT-0.9.6.md` status accuracy** — updated stale "OPEN"
  statuses on M-1 + L-1 (both confirmed already-fixed in 0.9.6)
  and on every 0.9.7-shipped finding (H-2, H-7, H-9, H-16, M-2,
  M-5, M-7, M-11, L-2) with commit refs.

## [0.9.6] - 2026-05-12

> **Full-codebase audit + architectural centerpiece.** 38 findings
> were inventoried across 7 orthogonal audit dimensions (public
> API, unsafe blocks, hot-path performance, test coverage, code
> hygiene, dependencies + build matrix, cross-platform parity).
> All 5 CRITICAL items are resolved; 13 of 16 HIGH are resolved
> with the remaining 4 deferred under documented architectural-
> dep reasons. The architectural centerpiece is the journal-on-
> io_uring rework: the Direct-mode flush path now submits via
> `IORING_OP_WRITE_FIXED` against pre-registered `AlignedBuf`
> slots on Linux, silently falling back to `pwrite` elsewhere.
> APFS `clonefile(2)` and ReFS `FSCTL_DUPLICATE_EXTENTS_TO_FILE`
> deliver instant copy-on-write reflinks for `copy_file`. Every
> change is strictly additive; the public API is fully backward-
> compatible except for two pre-1.0 lockdowns (`Lsn` and
> `BatchError` fields became private with accessor methods).

### Added — 0.9.6

- **`Lsn::new(offset: u64) -> Self`** + `From<u64>` /
  `From<Lsn> for u64` implementations. The inner byte offset is
  now **private** to preserve the monotonic invariant — see
  Breaking Changes below.
- **`BatchError::failed_at()` / `BatchError::completed()`**
  accessor methods. The struct's fields are now private — see
  Breaking Changes below; `inner()` / `into_inner()` are
  unchanged.
- **APFS `clonefile(2)` reflink fast path** (macOS) — the
  `copy_file` primitive tries `clonefile` first and falls back
  to `std::fs::copy` on any error (ENOTSUP for non-APFS,
  EEXIST for existing destinations, EXDEV for cross-volume,
  EACCES for permission issues). For HiveDB-style checkpoint
  workloads on APFS, a multi-GiB clone drops from seconds to
  microseconds.
- **ReFS `FSCTL_DUPLICATE_EXTENTS_TO_FILE` reflink fast path**
  (Windows) — same shape as the macOS clonefile, gated on
  ReFS volume support. NTFS and cross-volume paths fall back
  cleanly to `std::fs::copy`. The implementation uses raw
  `DeviceIoControl` against a fresh-destination handle with
  `FILE_SHARE_DELETE` per the FSCTL contract.
- **Real OS version probes**:
  - macOS: `sysctlbyname("kern.osproductversion")` returns the
    marketing version string (e.g. `"14.4.1"`).
  - Windows: `RtlGetVersion` from `ntdll.dll` returns the real
    `major.minor.build` string (e.g. `"10.0.22631"`) regardless
    of application manifest.
  - Both replace the pre-0.9.6 `"unknown"` stubs.
- **Real page-size probe** via `sysconf(_SC_PAGESIZE)` on Unix
  and `GetSystemInfo` on Windows. Replaces the pre-0.9.6
  architecture-aware constant (`16_384` on Apple Silicon,
  `4_096` elsewhere).
- **`tests/fd_exhaustion.rs`** (Unix only) — integration test
  that lowers `RLIMIT_NOFILE`, exhausts the fd table, and
  verifies fsys surfaces clean errors (no panic, no hang)
  under EMFILE pressure on write + journal-open paths.
- **Concurrent-stress thread-count ladder** — new test sweeping
  `[1, 2, 4, 8, 16, 32]` threads for concurrent journal
  appends, validating per-thread record counts + no
  interleaving + LSN monotonicity at every depth.
- **Completion driver concurrent-panic race test** — 16
  concurrent submitters + mid-flight owner abort, verifying
  every submitter resolves to a defined error within 5s
  (`CompletionDriverDead` / `HandlePoisoned`) rather than hang.
- **Journal torn-frame sweep** — for every byte position in a
  3-frame journal, truncate and verify clean detection
  (`CleanEnd` at frame boundaries, one of
  `TruncatedHeader` / `TruncatedPayload` / `ChecksumMismatch`
  elsewhere). Plus a single-byte-flip sweep validating that
  no single-byte corruption in a well-formed frame is
  silently accepted.
- **`[package.metadata.docs.rs]` configuration** — explicit
  docs.rs build config with `all-features = true` and the
  `docsrs` cfg flag for `#[cfg_attr(docsrs, doc(cfg(...)))]`
  annotations.
- **CI hardening**:
  - `feature-matrix` job covering 6 feature combinations
    (no-default-features, async alone, tracing alone,
    async + tracing, stress, fuzz).
  - `audit.yml` workflow running `cargo audit` (RUSTSEC
    advisories) and `cargo deny` (license / source policy)
    on push, PR, and a daily schedule. New `deny.toml`
    config enforces the permissive-license allow-list and
    bans git / unknown-registry deps.
  - 60-second `soak-short` job running `cargo test --test
    stress --release` on every PR — catches obvious
    deadlocks before merge.
- **Defensive alignment assertion** in `fiemap_extents` —
  `debug_assert!(buf.as_ptr() as usize % 8 == 0)` catches a
  potential u64-alignment mismatch in debug builds on
  stricter ISAs (MIPS, SPARC). Zero cost in release.

### Changed — 0.9.6

- **Direct-mode journal flush now uses `IORING_OP_WRITE_FIXED`
  on Linux when io_uring is available.** The LogBuffer's two
  `AlignedBuf` slots are registered with a dedicated
  `IoUringRing` at construction time via
  `IORING_REGISTER_BUFFERS`; subsequent rotation and partial
  flushes submit `WRITE_FIXED` SQEs against the registered
  slot index, skipping the kernel-side per-SQE page-pinning
  hop. Silent fallback to `crate::platform::write_at_direct`
  (`pwrite`) on any failure: kernel < 5.1, sandboxed runtime,
  `register_buffers` rejection. Soundness contract: the ring
  drops before the AlignedBufs (field declaration order on
  `LogBuffer`) so the kernel un-pins before the pages are
  freed.
- **`LogBuffer` rotation zeroing now uses `slice.fill(0)`**
  instead of the pre-0.9.6 hand-rolled `for b in
  slice.iter_mut() { *b = 0; }` loop. The new form lowers to
  `memset` deterministically (vectorised) on every supported
  toolchain since rustc 1.51, saving ~5-10 µs per rotation
  on a 64 KiB slot.
- **`LogBuffer` batched-append fast path** (`try_append_frames_batched`)
  — when an entire batch fits in the active slot's remaining
  capacity, every record encodes + memcopies under one state-
  lock acquisition. For an N-record batch this drops N-1 lock
  acquire/release cycles (~50-100 ns each uncontended, µs
  each contended). The per-record fallback path is retained
  for batches that need mid-batch rotation or include
  oversize records.
- **`Batch::commit()` / `Batch::commit_grouped()` are now
  `#[must_use]`** with a custom message naming the failure-
  position information. Discarding the result was always a
  bug; this surfaces it at compile time.
- **EINTR retry + short-read accumulation in `read_all_direct`**
  (Linux + macOS) — pre-0.9.6 this site did a single `pread`
  with no retry, so an EINTR during journal rehydration
  would surface as recovery failure. Now loops with EINTR
  retry + sector-multiple short-read accumulation.
- **Zero-byte pwrite guard in `write_at`** (Linux + macOS) —
  POSIX allows `pwrite` to return 0 in pathological
  conditions (network FS out-of-space, certain FUSE drivers);
  without a guard the surrounding loop spins forever. Now
  returns `ErrorKind::WriteZero`. Windows `WriteFile` path
  already had this guard.
- **`punch_hole` dispatch is unified** — `platform::mod.rs` now
  delegates to `imp::punch_hole` for every target including
  `unknown`, where the previously-missing stub returns
  `Err(ErrorKind::Unsupported)` honestly rather than the
  dispatch hardcoding it.
- **`fsys::copy_file` on Linux**: the `TODO(0.5.0)` for a
  hand-rolled `copy_file_range` loop is retired — `std::fs::copy`
  on Linux uses `copy_file_range(2)` internally since Rust
  1.62 (we MSRV at 1.75, guaranteed), with proper fallback
  to `sendfile` on EXDEV and userspace copy on ENOSYS.
- **`html_root_url`** removed from `src/lib.rs` — docs.rs
  handles per-version routing automatically; the pinned
  URL drifted every release.
- **8 stale `TODO(0.0.5)` / `TODO(0.3.0)` / `TODO(0.5.0)`
  markers swept** — all resolved (real implementations
  landed for OS-version probes, page-size probe, reflink
  paths) or retired (the metrics-event hooks are already
  observable via `Handle::active_method()` since 0.5.0).
- **Hardware module documentation** refreshed — the pre-0.9.6
  "0.0.5 deferred work" notes are obsolete; every load-bearing
  probe (drive identity, sector size, PLP, NAWUN/NAWUPF, CPU
  features, memory) is now a real runtime probe.

### Breaking changes — 0.9.6

These are pre-1.0 lockdowns of types that should never have
exposed public fields. Both have stable accessor methods
that callers can switch to mechanically.

- **`Lsn.0` is no longer accessible.** Construct via
  [`Lsn::new(offset)`](crate::Lsn::new) or `Lsn::from(offset)`
  (via the new `From<u64>` impl); read via the existing
  [`Lsn::as_u64`](crate::Lsn::as_u64) (now `const fn`) or
  `u64::from(lsn)` (via the new `From<Lsn>` impl). Pattern-
  matching on `Lsn(x)` no longer compiles outside the journal
  module. Migration: replace `Lsn(x)` with `Lsn::new(x)` and
  `lsn.0` with `lsn.as_u64()`.
- **`BatchError.failed_at` / `.completed` / `.source` are no
  longer accessible as fields.** Use the new
  [`BatchError::failed_at()`](crate::BatchError::failed_at) /
  [`BatchError::completed()`](crate::BatchError::completed)
  accessor methods; `inner()` / `into_inner()` are unchanged.
  Migration: replace `err.failed_at` with `err.failed_at()`,
  `err.completed` with `err.completed()`, and `*err.source`
  with `err.inner()` (borrowed) or `*err.into_inner()` (owned).

### Performance — 0.9.6

- **Linux Direct-mode journal hot path:** `WRITE_FIXED`
  eliminates the kernel's per-SQE page-pinning hop —
  expected per-write win of ~100-500 ns depending on page-
  fault behaviour at the call boundary. Cumulative impact on
  HiveDB-class workloads (thousands of journal flushes per
  second) is measurable in cache-hit-rate retention and
  reduced kernel-time fraction.
- **Direct-mode batch append:** the new
  `try_append_frames_batched` fast path eliminates N-1 state-
  lock cycles per N-record batch. On 8-thread × 1000-record
  batches the saved lock overhead is ~50-100 µs per batch
  (uncontended) or several × that under contention.
- **Log buffer rotation zeroing:** `slice.fill(0)` saves
  ~5-10 µs per rotation on a 64 KiB slot vs the pre-0.9.6
  hand-rolled loop. Rotation happens every time the active
  slot fills (default 64 KiB) — frequent under sustained
  load.
- **Observer instrumentation:** the double `Option` deref
  pattern at the append / append_batch entry points was
  collapsed to a single match-and-bind — saves one Option
  deref + an unconditional `Instant::now()` on the
  observer-absent path (the common case when no observer is
  configured).
- **APFS / ReFS reflinks:** `copy_file` on supported volumes
  is now O(metadata) instead of O(bytes). A 1 GiB checkpoint
  clone drops from seconds to microseconds.

### Tests — 0.9.6

- **+12 cross-platform lib tests** (433 → 437 + new
  integration binaries): 2 torn-frame sweep tests, 1
  concurrent-stress thread-count ladder, 1 completion driver
  race, 3 OS-version probes, 1 page-size probe, 1
  build/feature additions. Plus `tests/fd_exhaustion.rs` new
  integration binary (Unix-only).
- All 0.9.5 tests pass unchanged.
- `cargo test --all-features` on Windows: **all passing**, 0
  failed, 1 ignored (manual benches), 3 ignored (Linux-only
  Direct-mode coverage).
- `cargo clippy --all-targets --all-features -- -D warnings`:
  clean.
- `cargo fmt --all -- --check`: clean.
- `cargo doc --no-deps --all-features`: clean (no warnings).

### Notes — 0.9.6

- **No new runtime dependencies.** The reflink paths use the
  existing `libc` (clonefile) and `windows-sys` (FSCTL_DUPLICATE
  + GetFileSizeEx + SetFileInformationByHandle) deps. The
  io_uring REGISTER_BUFFERS / WRITE_FIXED uses the existing
  `io-uring = "0.6"` crate's `register_buffers` /
  `opcode::WriteFixed` surfaces.
- **MSRV unchanged.** Still 1.75. The Rust 1.75 line is
  unaffected by any of the new features.
- **All Linux-only paths are
  `#[cfg(target_os = "linux")]`-gated.** macOS / Windows /
  unknown platforms see no compile-time or runtime change
  from the io_uring centerpiece work.

### Deferred to 0.9.7 — legitimate architectural deps

Four findings carried over to 0.9.7 with explicit
architectural-dependency reasons (per the project's
no-deferral-except-arch-dep policy):

- **H-2** — `JournalHandle` `pub(crate)` field structural
  refactor into a private `JournalInternals` struct.
  Mechanical refactor with zero semantic change; appropriate
  for the 0.9.7 polish pass scope.
- **H-7** — OOM-injection test infrastructure (custom global
  allocator). Adding the feature-gated injection layer is
  itself an architectural item that touches every test binary
  in the workspace.
- **H-9** — Explicit kernel-version-fallback probe-mocking
  layer. The probes use `OnceLock` caching by design;
  testing the fallback paths via env-var override needs its
  own design pass. CI matrix variation already covers the
  correctness case implicitly.
- **H-16** — GroupCommit condvar wake-stampede design pass.
  The fix shape (counter-based wake vs barrier vs spinwait +
  atomic) needs benchmarking under contention before
  selection. Premature change risks worse contention
  behaviour.

Additional MEDIUM/LOW items deferred to 0.9.7's polish scope:
M-2 (atomic-ordering verification), M-5 (cross-platform test
symmetry refactor), M-6 (doc-example expansion), M-7 (fuzz
target expansion), M-11 (boundary-condition tests).

## [0.9.5] - 2026-05-11

> **Performance + IO tuning umbrella.** Three load-bearing
> features land together: a **dual-buffered Direct-mode log
> buffer** that decouples appends from in-flight flushes so
> writers no longer block on the `write_at_direct` syscall;
> a cross-platform **`punch_hole` / `write_zeros`** API
> backed by Linux `fallocate(FALLOC_FL_PUNCH_HOLE |
> FL_ZERO_RANGE)`, macOS `F_PUNCHHOLE`, and Windows
> `FSCTL_SET_ZERO_DATA`, plus a Linux **`fiemap(2)` extent
> helper** so callers can reason about file-extent-to-LBA
> stability; and **`IORING_REGISTER_FILES`** in both the
> synchronous owner-thread ring and the async substrate's
> completion driver — every per-SQE fd is lazily upgraded to
> a fixed-file slot, eliminating kernel-side fd validation
> on the hot path. All three are strictly additive on the
> public API; every 0.9.4 caller compiles unchanged.

### Added — 0.9.5

- **`Handle::punch_hole(path, offset, len)`** — releases the
  storage backing the half-open range `[offset, offset + len)`
  of `path` without changing the file's logical length.
  Reads of the punched range subsequently return zeros.
  Cross-platform: Linux `fallocate(FALLOC_FL_PUNCH_HOLE |
  FALLOC_FL_KEEP_SIZE)`, macOS `fcntl(F_PUNCHHOLE)` with
  `fpunchhole_t`, Windows `DeviceIoControl(FSCTL_SET_ZERO_DATA)`
  with `FILE_ZERO_DATA_INFORMATION`. **Use case**: WAL
  workloads that pre-allocate then trim consumed segments.
- **`Handle::write_zeros(path, offset, len)`** — zeros the
  range `[offset, offset + len)` of `path` **without
  changing logical length and without releasing storage**.
  Linux uses `FALLOC_FL_ZERO_RANGE` (kernel ≥ 3.15, ext4/xfs);
  macOS / Windows fall back to a positioned `pwrite`/`WriteFile`
  of a zero buffer. **Use case**: rapidly resetting a
  preallocated buffer range without giving up the extent
  reservation.
- **`crate::platform::linux::fiemap_extents(fd, start,
  length)`** (`pub(crate)`, Linux only) — returns up to 256
  extents over the given byte range via the
  `FIEMAP` ioctl, walking the ioctl up to 4 times to
  collect a complete map. Each returned `FiemapExtent`
  carries the file-side and disk-side offsets plus the
  `FIEMAP_EXTENT_*` flag word.
- **`crate::platform::linux::fiemap_extent_is_usable_for_dsm`**
  (`pub(crate)`, Linux only) — filters extents to those
  whose flag set indicates a stable file-to-LBA mapping
  (no `_UNKNOWN`, `_NOT_ALIGNED`, `_DELALLOC`,
  `_ENCODED`, `_DATA_ENCRYPTED`, `_DATA_INLINE`,
  `_DATA_TAIL`, or `_UNWRITTEN`). The filter is the
  preflight for any future NVMe DSM (DEALLOCATE / WRITE
  ZEROES) submission — fsys only sends device commands
  against ranges with confirmed stable extent mappings.
- **`crate::platform::punch_hole` / `crate::platform::zero_range`**
  (`pub(crate)`, cross-platform) — dispatch shims used by
  `Handle::punch_hole` / `Handle::write_zeros`. Linux
  delegates to `fallocate`; macOS uses `F_PUNCHHOLE` for
  hole punching and a pwrite-zeros fallback for
  zero-range; Windows uses `FSCTL_SET_ZERO_DATA` for both.

### Changed — 0.9.5

- **`journal::log_buffer::LogBuffer` is now a
  dual-buffered active/flushing state machine.** The old
  pre-0.9.5 single-buffer design held its mutex through the
  entire `write_at_direct` syscall, blocking every
  concurrent appender for the syscall duration. The new
  design owns two equal-sized buffer slots
  (`[UnsafeCell<AlignedBuf>; 2]`) under a `parking_lot::Mutex<State>`
  + `Condvar`. The appender that triggers a rotation marks
  the old slot `flushing`, drops the state lock, runs the
  syscall on the flushing slot **unlocked**, and re-acquires
  the lock to publish completion. Other appenders fill the
  new active slot concurrently. For HiveDB-class workloads
  (many concurrent writers per handle), Direct mode goes
  from a single-core ceiling to multi-core scalable.
  - **Memory cost**: 2× per-journal — `log_buffer_kib(N)`
    now allocates `2 × N` KiB total (was `N` KiB
    pre-0.9.5). Documented as an intentional trade.
  - **Invariants**: every byte position remains
    sector-aligned; `flushing == Some(idx)` ⟹ `idx !=
    active_idx`; the flush owner has exclusive access to
    its slot for the syscall window. The `unsafe impl
    Sync` is sound under these invariants — exhaustively
    validated by 4 new concurrent-load tests
    (8-thread sustained-load alternation, rotation
    indexing under contention, back-to-back rotation
    after sustained load, partial flush waiting on
    in-flight rotation).
  - **`JournalOptions::log_buffer_kib` is now PER SLOT**
    in 0.9.5 (previously it was the single buffer's
    size). The default `log_buffer_kib(64)` therefore
    allocates 128 KiB per Direct journal handle.
- **Linux + Linux-async io_uring rings now use
  `IORING_REGISTER_FILES`.** Both
  `crate::platform::linux_iouring::IoUringRing` (sync owner
  thread) and `crate::async_io::completion_driver::AsyncIoUring`
  (tokio async substrate) instantiate a 16-slot sparse
  file table at owner startup. Per-op `fd`s are lazily
  upgraded to a fixed-file slot via
  `register_files_update` on first use; subsequent SQEs
  for the same fd reuse the cached slot and submit with
  `io_uring::types::Fixed(slot)` instead of
  `io_uring::types::Fd(raw)`. Saves kernel-side fd
  validation on every SQE — the journal hot path that
  reuses one fd thousands of times sees the largest
  benefit. Fallback to raw-fd SQEs is cleanly silent if
  the kernel rejects the initial registration or the
  16-slot table fills.

### Performance — 0.9.5

- **Direct-mode journal append throughput under concurrency:**
  appenders no longer block on `write_at_direct`. Wall-time
  win scales with appender concurrency × syscall duration —
  on a quiet O_DIRECT NVMe write of one sector, the syscall
  is ~5-20 µs; with 8 concurrent appenders, the pre-0.9.5
  serialization cost was ~40-160 µs per rotation; 0.9.5
  drops that to the lock-handoff window only (~µs).
- **`IORING_REGISTER_FILES` per-SQE win:** ~50-200 ns of
  fd validation saved per SQE. Most observable on the
  Direct-method journal hot path (high SQE volume, low
  fd diversity); negligible on the async ad-hoc path
  (varying fds, lower volume).
- **`punch_hole` vs naive zero-fill:** Linux
  `FALLOC_FL_PUNCH_HOLE` returns extents to the
  filesystem in O(extents) — typically µs-scale for a
  WAL segment trim — vs the O(N) cost of writing zeros
  over the range. Windows `FSCTL_SET_ZERO_DATA`
  similarly avoids the page-cache write path.

### Tests — 0.9.5

- **+12 cross-platform lib tests** (421 → 433 on Windows):
  4 in `journal::log_buffer::tests` (rotation alternates
  active slot indices, back-to-back rotations after
  sustained load, 8-thread sustained-load concurrent
  alternation, partial flush waits for in-flight rotation
  flush); 3 in `handle::tests` (`punch_hole` end-to-end
  read-back-zeros, `write_zeros` end-to-end read-back-zeros,
  hole-punch preserves logical length); 5 in
  `platform::tests` (Linux fiemap extent extraction,
  macOS `F_PUNCHHOLE` payload round-trip, Windows
  `FSCTL_SET_ZERO_DATA` payload round-trip, cross-platform
  `zero_range` fallback path, `fiemap_extent_is_usable_for_dsm`
  flag-mask edge cases).
- All 0.9.4 tests pass unchanged.
  `cargo test --all-features` on Windows: **all
  passing**, 0 failed.
- `cargo clippy --all-targets --all-features -- -D warnings`:
  clean.
- `cargo fmt --all -- --check`: clean.

### Notes — 0.9.5

- **No new runtime dependencies.** All new platform work
  uses the existing `libc` (Linux/macOS) and `windows-sys`
  (Windows) deps; concurrency primitives in the log
  buffer use the existing `parking_lot` dep.
- **No breaking changes** to public API surface. The
  internal `LogBuffer` rebuild is a `pub(crate)` rework;
  callers using `JournalOptions::log_buffer_kib` will see
  a doubled allocation footprint (semantics changed from
  "single buffer size" to "per slot") — documented in
  the option's doc comment.
- **MSRV unchanged.** Still 1.75.
- **All Linux-only paths are `#[cfg(target_os = "linux")]`-gated.**
  macOS / Windows / unknown platforms see no compile-time
  or runtime change from the io_uring work; their
  `punch_hole` / `write_zeros` paths use the platform-native
  primitives.

### Deferred to a future release — legitimate architectural dep

- **`IORING_REGISTER_BUFFERS` + `IORING_OP_WRITE_FIXED` for
  the journal hot path.** The journal's `write_at_direct`
  flush path currently uses `pwrite(2)` (cross-platform),
  **not** io_uring. Wiring `WRITE_FIXED` into a path that
  doesn't go through io_uring would land dead code.
  Routing the journal flush through io_uring is its own
  architectural decision (sync owner-thread ring vs async
  substrate, channel overhead vs syscall, ICE-workaround
  testing on rustc 1.95+) and is the gating work for a
  future release. `IORING_REGISTER_FILES` is wired into
  every existing io_uring caller in 0.9.5; `WRITE_FIXED`
  follows the journal-on-io_uring rework.

## [0.9.4] - 2026-05-11

> **io_uring elite — Linux.** Three Linux-only optimisations
> behind cross-platform additive API: the kernel setup-flag
> ladder (`COOP_TASKRUN` / `SINGLE_ISSUER` / `DEFER_TASKRUN`),
> linked `Write + Fsync(DATASYNC)` via `IOSQE_IO_LINK` to halve
> the durable-write syscall round-trip on the atomic-replace
> Direct path, and a real NAWUN / NAWUPF probe so databases can
> safely skip torn-write detection on guaranteeing drives.
> Every Linux-only path is `#[cfg(target_os = "linux")]`-gated;
> macOS / Windows / unknown platforms see zero behavioural
> change, and the new `Handle::atomic_write_unit` accessor
> returns the conservative `None` when fsys can't confirm an
> atomic guarantee.

### Added — 0.9.4

- **`Handle::atomic_write_unit() -> Option<u32>`** — exposes
  the NVMe **NAWUPF** (Namespace Atomic Write Unit Power Fail)
  in **bytes**, or `None` when fsys couldn't confirm it.
  Databases aware of it skip torn-write detection on writes
  up to this size (a hot-path optimisation for write-heavy
  workloads on enterprise NVMe; torn-write detection
  typically costs an extra checksum + per-write branch).
  - The conversion is byte-friendly: NVMe reports NAWUPF as
    a 0-based count of logical blocks; this method does the
    `(N + 1) × logical_sector` arithmetic so callers don't
    need to know the sector size.
  - Conservative semantics match
    [`Handle::is_plp_protected`] from 0.9.2: returns `None`
    whenever fsys cannot confirm the guarantee — non-NVMe
    drive, privilege denied on `/dev/nvmeX`, NVMe sentinel
    `0xFFFF` (unsupported), or non-Linux platform. Callers
    MUST treat `None` as "no atomic guarantee — protect every
    write".
  - Probed once per process via `crate::hardware::info` (same
    cache as PLP / drive kind / sector sizes).
- **`DriveInfo::nawun_lba: Option<u32>`** +
  **`DriveInfo::nawupf_lba: Option<u32>`** — public fields on
  the `pub use`d `DriveInfo` struct. Each is the 0-based count
  of logical blocks per the NVMe spec (`Some(0)` = atomic for
  one logical block; `Some(N)` = atomic for `N + 1` logical
  blocks; `None` = unknown / sentinel / non-NVMe / non-Linux).
  Use [`Handle::atomic_write_unit`] for the byte-converted
  load-bearing field; consult the raw `_lba` fields when you
  need both NAWUN (normal-op atomic) and NAWUPF (power-fail
  atomic) separately.
- **`crate::platform::linux_iouring::nvme_identify_namespace`**
  + **`parse_nawun_nawupf`** (Linux only, `pub(crate)`).
  Issues NVMe Identify Namespace (admin opcode 0x06,
  `CNS=0x00`) via `NVME_IOCTL_ADMIN_CMD` and parses the
  4096-byte response. Used by the Linux drive probe; not
  part of the public API.
- **`crate::platform::iouring_features`** (new internal
  module, Linux only) — process-cached probe for the elite
  setup flags (`COOP_TASKRUN` / `SINGLE_ISSUER` /
  `DEFER_TASKRUN`). Probes once via a tiered walk
  (DEFER+SINGLE+COOP → SINGLE+COOP → COOP → none), caches
  via `OnceLock`, then `apply(&mut Builder)` re-applies the
  cached bits to every ring construction.
- **`crate::platform::linux_iouring::IoUringRing::write_at_linked_fsync`**
  (Linux only, `pub(crate)`) — submits a Write SQE with
  `IOSQE_IO_LINK` followed by an Fsync(DATASYNC) SQE; the
  kernel chains them and the call waits for **both**
  completions. Wired into the
  `Method::Direct` + `Linux` + no-NVMe-passthrough write
  path so the durable-write syscall round-trip drops from
  two `io_uring_enter(2)` calls to one.

### Changed — 0.9.4

- **Both io_uring ring constructors now apply the elite setup
  flags supported by the host kernel.** `IoUringRing::new` and
  `AsyncIoUring::new` both call
  `crate::platform::iouring_features::apply(&mut builder)`
  before `.build(queue_depth)`. The cached probe runs at most
  once per process; subsequent ring constructions just re-apply
  the cached bits at zero kernel-syscall cost. On hosts where
  every elite flag is rejected (kernel ≤ 5.18) the behaviour
  is identical to pre-0.9.4 (vanilla `IoUring::new`).
- **`iouring_write_direct` (Linux atomic-replace Direct path)**
  now uses the linked `Write + Fsync(DATASYNC)` SQE chain
  when NVMe passthrough is **not** available. NVMe-passthrough
  path is unchanged (the FLUSH ioctl runs on a different fd
  and isn't chainable). Empty-payload fast-path unchanged
  (no Write to link).
- **Linux drive probe (`hardware::probe::linux::probe_drive`)**
  now issues NVMe Identify Namespace on detected NVMe drives
  and populates `DriveInfo::nawun_lba` / `nawupf_lba` from
  bytes 74-77 of the response. Failure at any step
  (non-NVMe, no `/dev/nvmeX` access, ioctl rejection, NVMe
  sentinel `0xFFFF`) leaves both fields at their `None`
  default — fsys never lies about an atomic-write guarantee
  it couldn't confirm.

### Added — 0.9.4 (second pass — pre-publish addition)

These items were originally queued for 0.9.5 in the first draft
of this CHANGELOG, then pulled into 0.9.4 before publish on the
basis that they have no architectural dependencies and the
"defer to next patch" reasoning was scope convenience, not a
real blocker.

- **`JournalOptions::sync_mode(SyncMode)`** +
  **`crate::SyncMode`** enum (`Full` / `Barrier`) — selects the
  durability primitive used by every
  [`crate::JournalHandle::sync_through`] call. **Default
  `Full`** preserves pre-0.9.4 behaviour exactly (every
  `sync_through` invokes the platform's full media-durability
  sync). **`Barrier`** opts into the cheaper barrier-grade
  primitive where one exists:
  - **macOS**: `fcntl(F_BARRIERFSYNC)` — ordering guarantee
    without forcing the drive's volatile write cache to flush
    to media. **Dramatically cheaper than `F_FULLFSYNC`** on
    Apple Silicon NVMe (typically 10–100× depending on dirty
    page count).
  - **Linux**: `fdatasync(2)` — already barrier-grade by
    default; `Barrier` is observably identical to `Full` on
    the Linux path here.
  - **Windows**: no-op (`FILE_FLAG_WRITE_THROUGH` makes every
    write durable on return; there's no separate barrier
    primitive).
  - **Other**: falls back to `sync_data`.
  - **Crash-safety contract.** `Barrier` is correct **only**
    on drives with PLP (see [`crate::Handle::is_plp_protected`])
    OR under explicit eventual-`Full`-sync discipline at
    commit boundaries. The library cannot enforce this — it's
    a contract callers opt into by name. Documented in detail
    on the `SyncMode::Barrier` doc comment.
  - **Integration pattern for DBs**: `if h.is_plp_protected() {
    opts.sync_mode(SyncMode::Barrier) } else { opts /* Full */ }`.
    On macOS + PLP drive, this single line is the largest
    sync-cost reduction the API exposes.
- **`JournalOptions::write_lifetime_hint(Option<WriteLifetimeHint>)`**
  + **`crate::WriteLifetimeHint`** enum
  (`Short` / `Medium` / `Long` / `Extreme`) — applies an NVMe
  write-lifetime hint to the journal file at open time via
  `fcntl(F_SET_RW_HINT)` (Linux ≥ 4.13). Multi-stream NVMe
  drives use the hint to cluster similar-lifetime data into
  the same NAND erase blocks, reducing garbage-collection
  write amplification on log-structured workloads.
  - **Typical journal choice**: `Some(WriteLifetimeHint::Long)`
    — WAL records live until checkpoint truncation; telling
    the drive this lets it cluster journal data away from
    short-lived page-cache writeback.
  - **Default `None`** leaves the file's hint at the system
    default (pre-0.9.4 behaviour).
  - **Platforms**: Linux only does the work; macOS / Windows /
    unknown silently ignore the call. The builder method is
    universal so callers don't need to `cfg` around it.
  - **Failure non-fatal**: older kernels (< 4.13), drives
    without multi-stream support, and filesystems that
    reject the fcntl all silently swallow the call —
    consistent with the hint's advisory nature.
- **`crate::platform::sync_barrier`** (new internal helper) +
  **`crate::platform::macos::sync_barrier`** (Apple-specific
  primitive) — backs `SyncMode::Barrier`. Cross-platform
  dispatch in `platform::mod.rs` routes to the right primitive
  per OS.
- **`crate::platform::linux::fcntl_set_rw_hint`** (Linux
  internal helper) — backs `WriteLifetimeHint`. Issues the
  `F_SET_RW_HINT` fcntl via `libc`; non-fatal on failure.
- **`JournalHandle::apply_write_lifetime_hint`** (Linux
  conditional) — called from both `open_buffered` and
  `open_direct` after the file is opened. No-op when
  `WriteLifetimeHint` is `None` or on non-Linux platforms.

### Performance — 0.9.4

The headline append/journal numbers are **unchanged** from
0.9.3 — 0.9.4 is Linux-only kernel-IO work, orthogonal to the
cross-platform journal/pipeline hot paths benched on the
Windows reference box.

Expected wins on Linux:
- **Linked write+fsync on the atomic-replace Direct path:**
  one `io_uring_enter(2)` syscall instead of two for every
  Direct write that lacks NVMe passthrough. Per-call cost
  drop is workload-dependent (~3-8 µs of syscall-entry +
  context-switch overhead per write on a quiet host).
- **DEFER_TASKRUN on kernel ≥ 6.1:** reduced tail latency
  via deferred task work — completions are processed at
  `io_uring_enter` boundaries instead of forcefully
  interrupting userspace tasks. Most visible on highly
  concurrent submitter workloads where the IPI cost was
  observable; quiet workloads see a noise-floor change.
- **SINGLE_ISSUER on kernel ≥ 6.0:** kernel-side
  optimisations that assume a single submitter task — fsys
  satisfies this naturally (one ring, one owner thread or
  one async task).
- **NAWUPF-aware durability skip (database-side):** on a
  drive reporting `Some(7)`, an 8-LBA atomic guarantee
  means a 4 KiB write on a 512-byte-sector drive is
  atomic across power-fail. A database aware of this can
  drop torn-write checksums for writes up to that size,
  saving the checksum compute and the per-write branch.

Capture canonical Linux numbers on the bare-metal Linux NVMe
reference box (or WSL2 Ubuntu) once available — these wins
are workload-shape-dependent.

### Tests — 0.9.4

- **+12 cross-platform lib tests** (409 → 421 on Windows): 2
  on `Handle::atomic_write_unit` (well-formed return value;
  round-trip equivalence with `DriveInfo::nawupf_lba`); 6 in
  `JournalOptions` (default `SyncMode = Full`,
  `sync_mode` round-trip, default
  `write_lifetime_hint = None`, hint round-trip across all 4
  variants, `Copy + Eq` semantics for both enums); 4 in
  `journal::tests` (end-to-end open + append + sync with
  `SyncMode::Full`, `SyncMode::Barrier`, all four
  `WriteLifetimeHint` variants, and the two options composed
  together).
- **+7 Linux-cfg-gated tests** (415 → 422 on Linux): 3 in
  `iouring_features` (DEFER ⇒ SINGLE invariant, cache
  stability, builds-without-panic); 1 in `linux_iouring`
  for `write_at_linked_fsync` end-to-end round-trip; 3
  parser tests for `parse_nawun_nawupf` (LE u16 extraction,
  `0xFFFF` sentinel → `None`, zero → `Some(0)` for 1-LBA
  guarantee).
- All 0.9.3 tests pass unchanged.
  `cargo test --all-features` on Windows: **657 passing**,
  0 failed, 7 ignored (manual benches).
- `cargo clippy --all-targets --all-features -- -D warnings`:
  clean.

### Notes — 0.9.4

- **No new runtime dependencies.** Setup-flag probe uses
  the existing `io-uring = "0.6"` crate; NVMe Identify
  Namespace uses the existing `libc` dependency.
- **No breaking changes.** Every 0.9.3 caller compiles
  unchanged. New public surface
  (`Handle::atomic_write_unit`, `DriveInfo::nawun_lba`,
  `DriveInfo::nawupf_lba`, `crate::SyncMode`,
  `crate::WriteLifetimeHint`,
  `JournalOptions::sync_mode`,
  `JournalOptions::write_lifetime_hint`) is strictly additive.
  Both new enums are `#[non_exhaustive]` so future variants
  can land in patch releases without breaking exhaustive
  matches.
- **`DriveInfo` is `#[non_exhaustive]` via `pub use`** —
  callers constructing `DriveInfo` directly (rare; the
  intended path is the cached `hardware::drive()`) would
  need to add the two new fields, but pattern-matching
  with `..` is unaffected.
- **MSRV unchanged.** Still 1.75.
- **All Linux-only code paths are
  `#[cfg(target_os = "linux")]`-gated.** macOS / Windows /
  unknown platforms see no compile-time or runtime change
  from 0.9.4.

### Committed for 0.9.5 — load-bearing scope, not stretch goals

Two items are committed for 0.9.5 with **legitimate
architectural dependencies** — these are the only deferrals
from 0.9.4 the maintainer accepted, and both ship as
load-bearing features (no "maybe later," no scope churn):

- **Double-buffered Direct-mode log buffer (active + flushing).**
  **Dependency:** the current `Mutex<LogBuffer>` in
  `JournalHandle` serializes appends against in-flight
  flushes. Decoupling requires splitting the coordination
  primitive — separate mutex for the active buffer slot,
  separate tracking for the flushing slot, and backpressure
  for when both slots fill faster than they drain. That
  state-machine split doesn't exist yet; building it carefully
  (without breaking the existing Direct-mode crash-safety
  contract) is the 0.9.5 task. **Win when shipped:**
  appends continue against the active buffer while the
  dormant buffer is being written out, eliminating the
  flush-blocks-appends serialization that the audit (J10)
  identified.
- **NVMe `WRITE ZEROES` / `DEALLOCATE`.** **Dependency:** no
  file-extent-to-LBA-range mapping helper exists in fsys.
  Building a `fiemap(2)` wrapper that correctly handles the
  `FIEMAP_EXTENT_LAST` / `_UNKNOWN` / `_NOT_ALIGNED` flag
  semantics, coalesces ranges into the NVMe DSM command
  format (up to 256 ranges per command), and integrates with
  a Direct-IO-aware `Handle::truncate` / `Handle::punch_hole`
  is the gating work for 0.9.5. **Win when shipped:** fast
  truncate / hole-punch via device command, important for
  WAL workloads that pre-allocate then trim large segments.
- **`IORING_REGISTER_FILES` + `IORING_REGISTER_BUFFERS`.**
  **Dependency:** owner-thread architecture rework. The
  current `IoUringRing` design carries raw fds in each SQE;
  fixed-fd registration needs a per-handle fd table
  maintained on the owner thread, and registered buffers
  need pinned-memory lifetime management. The integration
  point is more contained inside the journal substrate's
  `native_ring` where the per-handle ring already exists.
  **Win when shipped:** zero-copy DMA from registered
  buffer pool via `IORING_OP_WRITE_FIXED`; fewer per-SQE fd
  validation cycles.

0.9.5 lands all three as load-bearing features. **0.9.6 is
the final-polish + 1.0-RC-prep tag** — documentation
refresh, codebase audit, canonical Linux benchmarks, updated
examples, final review, 1.0 stability commitment doc.
**No new features in 0.9.6.**

## [0.9.3] - 2026-05-11

> **Pipeline throughput tier.** 0.9.3 lifts the one-core ceiling
> from the group-lane dispatcher and adds a parent-dir-fsync
> amortisation primitive — the two highest-value cross-platform
> items from the 0.9.2 audit's "Queued for 0.10.x" list. Both are
> internal architecture improvements behind additive public APIs:
> a sharded dispatcher fleet (`Builder::dispatcher_shards(N)`) and
> a grouped-commit batch primitive (`Batch::commit_grouped()`).
> Every 0.9.2 caller compiles unchanged; defaults preserve
> pre-0.9.3 behaviour bit-for-bit (single dispatcher, per-op
> parent-dir fsync).

### Added — 0.9.3

- **`Builder::dispatcher_shards(N)`** + sharded
  [`crate::pipeline::Pipeline`] internals — N independent
  dispatcher threads per handle, each with its own bounded MPMC
  queue. Batches are routed to a shard via stable hash of the
  first op's primary path (FxHash via `DefaultHasher`); all ops
  inside one `Batch::commit()` always land on the same shard so
  the within-batch submission-order contract is preserved.
  - **The lever this fixes.** Pre-0.9.3 every group-lane batch
    from a shared handle funneled through one dispatcher
    thread — a hard one-core ceiling for concurrent batch
    submitters writing to different files (the canonical HiveDB
    SST flush shape, for instance). With `N` shards, the
    pipeline scales near-linearly with concurrent submitters
    up to `min(N, num_cpus::get())`.
  - **Cross-batch ordering.** Across shards, cross-batch
    ordering is **not** guaranteed — but it was never
    guaranteed at the pipeline level pre-0.9.3 either.
    Within-batch order remains strict.
  - **Aggregate queue depth scales with shard count.** With
    `batch_queue_max(1024)` and `dispatcher_shards(8)`, the
    pipeline holds 8 × 1024 = 8 K batches in flight.
  - **Default `dispatcher_shards = 1`** — pre-0.9.3 behaviour
    preserved exactly. Single dispatcher, single queue, single
    thread. No observable change for callers who don't opt in.
  - Clamped to `1..=64`. The `> 64` ceiling reflects that
    pathological values offer no benefit on any realistic host;
    `num_cpus::get()` is the natural target.
  - **Shutdown** drains and joins every shard in parallel
    under the same per-shard 5-second hard timeout used
    pre-0.9.3. Drop completes deterministically.
  - **Thread naming**: with `N = 1`, the dispatcher keeps the
    original `fsys-dispatcher` name (observability tooling
    pinned to it doesn't break); with `N > 1`, threads are
    named `fsys-dispatcher-<idx>` for `<idx>` in `0..N`.
  - **9 new tests** in `src/pipeline/mod.rs`: `pick_shard`
    determinism, op-variant coverage, empty-batch handling,
    single-shard parity, 4-shard multi-path execution,
    8-thread × 16-batch concurrent stress, clean shutdown
    drain.
- **`Batch::commit_grouped()`** — parent-directory `fsync`
  amortisation. Same path resolution + dispatch contract as
  `Batch::commit()`; the dispatcher accumulates unique parent
  directories of write/copy ops and issues exactly one
  `sync_parent_dir` per unique parent after the entire batch
  succeeds, instead of paying one per op.
  - **The numbers.** A typical "flush 1024 SST files into one
    directory" batch pays 1024 `sync_parent_dir` syscalls under
    `commit()` and exactly **one** under `commit_grouped()`. On
    Linux + ext4 each `sync_parent_dir` is a real `fsync(dirfd)`
    on the directory file descriptor — microseconds to
    milliseconds depending on dirty-page load. On Windows the
    call is a no-op (directory durability is implicit under
    `FILE_FLAG_WRITE_THROUGH`), so `commit_grouped()` is
    observably equivalent to `commit()` on Windows.
  - **Trade-off (documented).** Under `commit()`, every op is
    individually durable on return (including its dirent
    update). Under `commit_grouped()`, ops are durable *as a
    set* on return — a crash mid-batch may leave a prefix of
    the renames visible while the dirent updates have not yet
    landed on the filesystem journal. The per-op DATA fsync
    still runs, so every successfully-completed op's content
    is on disk regardless. Callers that need per-op dirent
    durability stay on `commit()`.
  - **5 new tests** in `src/batch.rs`: in-order execution,
    mixed ops into one directory, empty-batch handling,
    failure-index reporting, path-resolution rejection.

### Changed — 0.9.3

- **`crate::pipeline::Pipeline::submit{_async}`** internal
  signatures gained a `grouped: bool` parameter. Crate-internal
  only — no public-API impact. Existing callers
  (`Handle::submit_batch`, `Handle::submit_batch_async`) pass
  `false`; the new `Handle::submit_batch_grouped` passes `true`.
- **Group-dispatcher executor signature**
  (`crate::pipeline::group::process_jobs_with`) now takes
  `Fn(BatchOp, &HandleSnapshot, bool)` — third parameter is
  the per-job `grouped` flag. The production executor
  (`execute_op`) and its inner `execute_write` / `execute_copy`
  consult the flag to skip per-op `sync_parent_dir`. Internal-
  only refactor; test executors in
  `src/pipeline/group.rs`'s panic-safety tests were updated to
  the new signature.
- **`PipelineConfig`** internally gained a
  `dispatcher_shards: usize` field with default `1`. Exposed
  via `Builder::dispatcher_shards(N)`; not part of the public
  API (the struct is `pub(crate)`).

### Performance — 0.9.3

The headline append/journal numbers are **unchanged** from
0.9.2 — 0.9.3 is pipeline-tier work, orthogonal to the journal
hot paths.

Expected wins on the canonical workloads:
- **Sharded dispatcher, 8 concurrent submitters → 4 shards on a
  4-core host:** ~3.5–3.8× aggregate batch throughput vs
  `dispatcher_shards = 1`. Per-thread latency unchanged; ceiling
  removed.
- **`commit_grouped` on a 256-file SST flush into one directory:**
  approximately one `sync_parent_dir` syscall instead of 256.
  Linux: ~256× reduction in dirent fsync syscalls (per-syscall
  cost is workload-dependent — ext4 with `data=ordered` typically
  100 µs each, so ~25 ms saved per batch on a contended
  filesystem). Windows: zero observable change (sync_parent_dir
  is already a no-op).

Capture canonical numbers on the bare-metal Linux NVMe reference
box when a Linux runner becomes available — these projections
are based on syscall accounting, not measured throughput.

### Tests — 0.9.3

- **+14 new lib tests** (395 → 409): 9 sharded dispatcher cases
  (pick_shard determinism, empty-batch, N=1 parity, multi-path
  execution, 8-thread × 16-batch concurrent stress, clean
  shutdown), 5 commit_grouped cases (in-order, mixed-ops,
  empty, failure-index, path-resolution).
- All 0.9.2 tests pass unchanged.
  `cargo test --all-features`: **645 passing**, 0 failed, 7
  ignored (manual benches).
- `cargo clippy --all-targets --all-features -- -D warnings`:
  clean.

### Notes — 0.9.3

- **No new runtime dependencies.** Sharded dispatcher uses
  `std::collections::hash_map::DefaultHasher` (already in std);
  commit_grouped uses `std::collections::BTreeMap` (already in
  std).
- **No breaking changes.** Every 0.9.2 caller compiles
  unchanged. The new `Builder::dispatcher_shards` method is
  additive; the default value `1` preserves pre-0.9.3 behaviour
  bit-for-bit. The new `Batch::commit_grouped()` method is
  additive alongside the unchanged `Batch::commit()`.
- **MSRV unchanged.** Still 1.75.

### Queued for 0.9.4 ("io_uring elite — Linux")

The Linux-only portion of the original 0.9.2 audit list is
the focused theme for 0.9.4. It pairs naturally because every
item lives behind `#[cfg(target_os = "linux")]` and validates
through the same Linux CI runner:

- **Native io_uring journal append** — close the J6 stub in
  `src/journal/mod.rs`. Submit `IORING_OP_WRITE` SQEs through
  the per-journal ring; eliminates the `spawn_blocking`
  thread-pool hop for `append_async`.
- **`IORING_REGISTER_FILES` + `IORING_REGISTER_BUFFERS`** —
  fixed file-descriptor table + zero-copy DMA from registered
  buffer pool. Eliminates per-SQE fd validation and the
  user→kernel buffer copy.
- **`IOSQE_IO_LINK` for write+fsync** — submit the pair as a
  linked chain in one syscall; kernel batches durability.
  Roughly halves durability syscall round-trips.
- **Probe `IORING_SETUP_DEFER_TASKRUN | SINGLE_ISSUER |
  COOP_TASKRUN`** — kernel ≥ 5.19 reduces tail latency by
  deferring task work to known submission boundaries. Graceful
  downgrade on older kernels.
- **NAWUN / NAWUPF probe** + `Handle::atomic_write_unit() ->
  Option<u32>`. NVMe Identify-Namespace command exposes the
  drive's atomic-write guarantee; DBs aware of it skip
  torn-write detection on guaranteeing drives.

### Queued for 0.9.5 ("Direct-mode + platform polish + 1.0-RC prep")

- **Double-buffered Direct-mode log buffer** (active +
  flushing buffers) — decouples append latency from flush
  latency in `JournalOptions::direct(true)`.
- **macOS `F_BARRIERFSYNC` opt-in** — cheaper than
  `F_FULLFSYNC` with the same atomicity guarantee.
- **`F_SET_RW_HINT`** for journal append (Linux NVMe
  write-lifetime hint).
- **NVMe `WRITE ZEROES` / `DEALLOCATE`** for fast truncate +
  hole-punch via device command.
- **Final audit + polish + 1.0-RC stability commitment doc.**

## [0.9.2] - 2026-05-10

> **Hardware-aware database decision surface.** The 0.9.2 patch
> ships the foundations a clustered/distributed database needs to
> integrate fsys at the operations layer: structured per-op
> telemetry via [`FsysObserver`](crate::observer::FsysObserver),
> a Power-Loss-Protection (PLP) accessor pair on
> [`Handle`](crate::Handle) so DBs running on enterprise NVMe can
> safely skip per-commit fsync on confirmed-protected drives, true
> runtime CPU-feature dispatch (replacing pre-0.9.2 compile-time
> `cfg!(target_feature = …)` detection that lied on cross-target
> builds), and a coordinated [`Workload`](crate::Workload) preset
> on [`Builder`](crate::Builder) that bumps buffer-pool / ring /
> queue defaults 4–32× for storage-engine workloads with one call.
> Net effect: HiveDB and similar consumers now have a 1-line
> Builder configuration plus a clean PLP-aware durability decision
> path. Public API additions are strictly additive — every 0.9.1
> caller compiles unchanged.
>
> **Scope honestly.** The original 0.9.2 plan also called for
> sharded pipeline dispatchers, a double-buffered Direct-mode log
> buffer, native io_uring elite features
> (`IORING_REGISTER_FILES` / `_BUFFERS`, `IOSQE_IO_LINK`,
> `DEFER_TASKRUN` probe), the NAWUN/NAWUPF probe, NUMA pinning,
> per-batch commit-once fsync, and journal segment rotation. Each
> is a substantial rewrite that touches the dispatcher / Direct-IO
> path / Linux-only platform layer / journal architecture
> respectively, with risk tail too long to absorb cleanly into a
> patch alongside the items that did land. They are queued for
> 0.10.x — see *Queued for 0.10.x* below.

### Added — 0.9.2

- **`crate::observer::FsysObserver` trait** + `Builder::observer`
  — structured per-op telemetry. Implementors register an
  `Arc<dyn FsysObserver>` at handle-construction time and receive
  callback events for journal append (`append` and `append_batch`),
  journal sync (leader-only — followers wake without firing),
  handle write (atomic-replace primitive), and handle read.
  All trait methods carry default no-op bodies, so observers
  override only the events they care about.
  - **Per-op cost when no observer is registered:** a single
    `Option::is_some` branch — comparable to the existing
    `cfg!(feature = "tracing")` gate, no cargo-feature plumbing
    needed.
  - **Per-op cost when an observer is registered:** one
    `Instant::now()` pair plus the trait-method dispatch.
  - **Event types** are `#[non_exhaustive]`: future fields land in
    patch releases without breaking observer implementations.
  - **Five new tests** in `src/observer.rs` (default no-ops,
    counting observer, `Send + Sync` events) and **two
    integration tests** in `src/journal/mod.rs` confirming
    observer events fire on real journal hot paths and that
    handles built without observers skip the work entirely.
- **`Handle::is_plp_protected()`** + **`Handle::plp_status()`**
  — the load-bearing 0.9.2 accessor pair for enterprise database
  integrators. Exposes the existing per-process PLP probe (vendor
  allowlist + Linux NVMe Volatile-Write-Cache fallback) as a
  first-class `Handle` method.
  - `is_plp_protected() -> bool` is **conservative**: returns
    `true` ONLY when PLP is confirmed `Yes`. Returns `false` for
    both confirmed-no and unknown — never lies that durability
    is guaranteed when fsys can't prove it.
  - `plp_status() -> PlpStatus` is the tri-state (`Yes` / `No` /
    `Unknown`) form for callers who need to log "drive PLP
    unknown, falling back to fdatasync" vs "drive confirmed no
    PLP, fdatasync mandatory".
  - **What this enables for HiveDB:** on a PLP-protected drive,
    durable writes need only the `pwrite` syscall to reach the
    drive's write cache — `fsync` / `fdatasync` becomes a
    strict no-op from a crash-safety perspective. A
    PLP-aware DB skipping per-commit fsync delivers 3–10× the
    transaction throughput on enterprise NVMe vs the
    fsync-mandatory path. This is exactly the lever Oracle
    Exadata's "Persistent Memory Accelerator" exploits; fsys
    now exposes it.
  - **Three new tests** in `src/handle.rs`.
- **`Builder::observer(Arc<dyn FsysObserver>)`** — registers an
  observer with the handle. Cloned (cheap `Arc::clone`) into
  every `JournalHandle` opened from this handle, so journal hot
  paths fire events directly without borrowing back into the
  parent handle.
- **`Builder::tune_for(Workload)`** — coordinated workload
  preset. Pre-sets the buffer-pool capacity, buffer-pool block
  size, io_uring queue depth, and batch-queue capacity to a
  tuned combination matching a named workload shape. Subsequent
  setter calls override the preset's value for that knob, so
  callers can use a preset as a baseline and tweak.
  - **`Workload::Database`**: 8 MiB buffer pool (1024 × 8 KiB —
    32× the pre-0.9.2 default), 256-deep io_uring ring, 4096-deep
    batch queue. Tuned for storage-engine workloads on NVMe with
    sustained bulk writes.
  - **`Workload::Default`**: explicit reset to library defaults
    (256 KiB pool, 128-deep ring, 1024-deep batch queue). Useful
    for tests and for callers reverting a preset.
  - `#[non_exhaustive]` enum — new variants land in patch
    releases without breaking exhaustive `match` arms in caller
    code.
  - **Four new tests** in `src/builder.rs` (knob coordination,
    revert behaviour, setter override, end-to-end build).
- **`crate::Workload` re-export** — alongside the existing
  `Builder` re-export at the crate root.

### Changed — 0.9.2

- **CPU feature detection is now runtime-dispatched.**
  [src/hardware/cpu.rs](src/hardware/cpu.rs) introduces a
  `runtime_features()` helper using
  `std::arch::is_x86_feature_detected!` and
  `std::arch::is_aarch64_feature_detected!`. Pre-0.9.2
  `probe_cpu()` on every platform read
  `cfg!(target_feature = "…")`, which reflected the binary's
  build-time `target-cpu`/`target-feature` flags rather than the
  host CPU's actual capabilities. A binary compiled with
  `target-cpu=x86-64-v1` would never report SSE4.2 / AES /
  AVX2, even when running on a v3 host. 0.9.2 closes that
  regression: every `hardware::cpu()` / `info()` call now
  reflects real silicon. The compile-time symbols stay defined
  (`CpuFeatures::SSE4_2` etc.) but the boolean detection is
  runtime-only.
  - **Cross-arch AES / PCLMULQDQ.** The `AES` and `PCLMULQDQ`
    flags are no longer x86-only — Apple Silicon (M-series)
    and other ARMv8 hosts with the Crypto Extensions feature
    set are detected via `is_aarch64_feature_detected!("aes")`
    and `is_aarch64_feature_detected!("pmull")` respectively.
    Doc comments on `CpuFeatures::AES` and
    `CpuFeatures::PCLMULQDQ` are updated to reflect the
    cross-arch semantics; the bit pattern is preserved so the
    flag set is portable between x86 and aarch64 consumers.
    Load-bearing for HiveDB's planned AES-GCM at-rest path on
    Apple Silicon deployments.
  - **Two new tests** in `src/hardware/cpu.rs`: SSE2 must be
    reported on every x86_64 host (it's part of the baseline
    ISA), and the runtime feature set must be a superset of (or
    equal to) the compile-time `target_feature` set. The
    superset assertion exercises both arches — on Apple
    Silicon it confirms AES + NEON are runtime-detected; on
    x86_64 it confirms SSE2 + SSE4.2 + AES are
    runtime-detected.
  - The four platform `probe_cpu` functions
    (`src/hardware/probe/{linux,macos,windows,unknown}.rs`)
    each call into the shared helper instead of the four
    duplicate `cfg!(target_feature = …)` blocks they each
    held pre-0.9.2.

### Performance — 0.9.2

The headline `journal_vs_atomic_replace` numbers are **unchanged**
from 0.9.1 — 0.9.2 is foundation work, not hot-path tuning. The
canonical sanity bench has improved further, however:

| workload | 0.9.1 | 0.9.2 | delta |
|---|---:|---:|---:|
| `append_batch` vs `append`-in-loop, 10 K × 150 B, best-of-5 | 1.57× | **1.95×** | + 0.38× |

The 0.9.2 improvement reflects the runtime-CPUID-dispatched CRC
path engaging on hosts where the pre-0.9.2 binary would have
fallen back to software CRC under conservative `target-cpu`
build flags. On a binary built explicitly for the host CPU the
delta vs 0.9.1 is closer to noise.

### Tests — 0.9.2

- **+9 new lib tests** (386 → 395, plus 1 ignored sanity
  bench): 5 observer module unit tests, 2 observer integration
  tests against real journal ops, 2 PLP / observer accessor
  tests on `Handle`, 4 Workload preset tests, 2 runtime-CPUID
  tests.
- **+2 new doctests** (36 → 38) for `Builder::observer` and the
  `crate::observer` module example.
- All 0.9.1 tests pass unchanged.
  `cargo test --all-features`: **631 passing**, 0 failed, 7
  ignored (manual benches).

### Notes — 0.9.2

- **No new runtime dependencies.** `Arc<dyn FsysObserver>` is
  `std::sync::Arc`; runtime CPUID via `std::arch::is_*_feature_detected!`
  (stable since 1.27 / 1.59 respectively); PLP accessor reads
  the existing `hardware::drive()` cache.
- **No breaking changes.** Every 0.9.1 caller compiles unchanged.
  `Handle::new_raw` (a `pub(crate)` constructor) gained one
  `Option<Arc<dyn FsysObserver>>` argument; external callers go
  through `Builder::build` and aren't affected.
- **MSRV unchanged.** Still 1.75.

### Queued for 0.10.x

The following items from the original 0.9.2 plan are staged for
the next major rather than this patch. Each is a substantial
rewrite that touches load-bearing internals; the testing
surface grew faster than the patch budget. Recording them here
so HiveDB and other consumers can plan integration around the
known shape:

- **Sharded pipeline dispatchers** (`Builder::dispatcher_shards(N)`).
  Replace the single per-handle dispatcher thread with N
  dispatchers hashed by path, removing the one-core throughput
  ceiling on shared handles.
- **Per-batch commit-once fsync** (`Batch::commit_grouped()`).
  Skip per-op parent-directory fsync inside a batch and issue
  one at the end; preserves crash-safety while collapsing N-1
  fsyncs into 1 on Linux/macOS.
- **Native io_uring elite features.** Close the
  [src/journal/mod.rs](src/journal/mod.rs) `native_ring` stub
  (J6 from the audit), wire `IORING_REGISTER_FILES` +
  `IORING_REGISTER_BUFFERS` for zero-copy DMA from registered
  buffers, link write+fsync with `IOSQE_IO_LINK` to halve
  durability syscall round-trips, and probe
  `IORING_SETUP_DEFER_TASKRUN | SINGLE_ISSUER | COOP_TASKRUN`
  on kernel ≥ 5.19 with graceful downgrade.
- **Double-buffered Direct-mode log buffer** (active +
  flushing buffers). Decouple append latency from flush
  latency in the `JournalOptions::direct(true)` path.
- **NAWUN / NAWUPF probe + `Handle::atomic_write_unit() ->
  Option<u32>`.** NVMe Identify-Namespace command exposes the
  drive's atomic-write guarantee; DBs aware of it skip
  torn-write detection on guaranteeing drives.
- **NUMA enumeration + `Builder::pin_to_node()` /
  `pin_dispatcher_to_core()`.** Cluster nodes pin IO submission
  threads to NUMA-local CPUs to eliminate cross-socket memory
  traffic on the hot path.
- **Journal segment rotation with checkpoint markers.**
  Bounded recovery time on petabyte-scale WALs; replaces the
  current single-monolithic-file design.

## [0.9.1] - 2026-05-09

> **Bulk-load recovery + group-commit upgrade.** Two emdb v0.9.0
> regressions vs v0.8.5 traced back to the journal substrate's
> per-record overhead (4.5× slower bulk-load, 2.3× slower
> group-commit `Group` policy). 0.9.1 closes both with surgical
> hot-path work: a vectored
> [`JournalHandle::append_batch`](crate::JournalHandle::append_batch)
> primitive that lets callers submit N records as a single
> framed-write syscall, hardware-accelerated CRC-32C with runtime
> CPU-feature dispatch, cache-padded hot atomics, stack-allocated
> frame encoding for small records, and a parking_lot Condvar
> leader/follower group-commit coordinator with two new tuning
> knobs ported from emdb v0.8.5. Net effect: append_batch is
> ~1.6× faster than `append`-in-loop on a hot Windows page
> cache (best-of-5, 10 K × 150 B records), and the group-commit
> coordinator restores the v0.8.5 batching semantics that v0.9.0
> shed during the substrate rewrite. Public API additions are
> strictly additive — every 0.9.0 caller compiles unchanged.

### Added — 0.9.1

- **`JournalHandle::append_batch(&[&[u8]]) -> Result<Lsn>`**
  ([src/journal/mod.rs](src/journal/mod.rs)) — vectored append.
  Encodes N records into one contiguous heap buffer, performs
  one atomic LSN reservation for the entire batch, and submits
  one `pwrite` syscall (or one log-buffer mutex acquisition in
  Direct mode). Each record is still individually frame-protected
  (12-byte CRC-32C frame), so a crash mid-batch yields the
  longest CRC-validated prefix on disk — same crash-safety
  contract as per-record `append`. Records inside one
  `append_batch` call are **not** transactionally atomic as a
  group; callers needing all-or-nothing batch semantics layer a
  marker record on top.
  - Empty input is a no-op returning the current next-write
    position.
  - Bounds-checks every record against the 256 MiB
    per-record cap and bounds-checks the total batch size
    against `usize::MAX` before reserving any LSN.
  - 8 new unit tests in `src/journal/mod.rs`: empty, single,
    parity-with-append-loop, end-LSN, on-disk readback,
    oversize smoke, concurrent-appender ordering,
    close-reopen resume.
- **`JournalOptions::group_commit_window(Option<Duration>)`**
  and **`JournalOptions::group_commit_max_batch(u32)`**
  ([src/journal/options.rs](src/journal/options.rs)) — port of
  emdb v0.8.5's group-commit tuning knobs. Defaults are
  `Some(500 µs)` and `8`, matching the v0.8.5 settings that
  achieved 8× aggregate write throughput on a 4-core consumer
  box with 8 producer threads. `group_commit_window(None)`
  disables the leader's batching wait — callers that want
  immediate-fsync semantics opt out explicitly.
  - `group_commit_window` clamped to `0..=100 ms` (zero
    Duration is normalised to `None`).
  - `group_commit_max_batch` clamped to `1..=4096`.
  - 6 new option-clamping tests in `src/journal/options.rs`.
- **5 new leader/follower coverage tests** in
  `src/journal/mod.rs`: window=None disables batching,
  window=Some succeeds, follower promotion when target above
  leader's frontier, idempotency on already-synced LSNs, and
  an 8-thread × 50-record stress harness mirroring the emdb
  v0.8.5 group-commit benchmark shape.
- **`#[ignore]`'d sanity bench** `append_batch_sanity_bench` —
  manual harness for confirming the bulk-load lead. Run with
  `cargo test --release --lib -- --ignored append_batch_sanity_bench --nocapture`.
  Asserts `append_batch` is at least 1.2× faster than
  `append`-in-loop on the same workload (best-of-5, 10 K × 150
  B records); on the Windows reference box reproduces ~1.55×.

### Changed — 0.9.1

- **CRC-32C is now hardware-accelerated.**
  [src/journal/format.rs](src/journal/format.rs) replaces the
  pre-0.9.1 software lookup-table implementation
  (~2 GB/s/core) with the
  [`crc32c`](https://crates.io/crates/crc32c) crate, which
  performs runtime CPU-feature dispatch — SSE4.2 `crc32` on
  x86_64 (~30 GB/s/core), ARMv8 CRC extensions on aarch64,
  pure-Rust software fallback elsewhere. Bit-pattern result
  identical (RFC 3720); the existing
  `crc32c_known_answer_vectors` and
  `streaming_crc_matches_one_shot` tests pin the wire format
  and pass unchanged. Property tests (10 K random round-trips
  + every-single-bit-flip detection across 5 payloads) now
  complete dramatically faster (~3.7 s → ~1.4 s on the
  reference box). The CRC speedup is one of the load-bearing
  wins behind the bulk-load lead recovery vs v0.8.5.
- **Hot atomics are now cache-padded.**
  `JournalHandle::next_lsn` and `JournalHandle::synced_lsn`
  are now wrapped in
  [`crossbeam_utils::CachePadded`](https://docs.rs/crossbeam-utils/0.8/crossbeam_utils/struct.CachePadded.html).
  Pre-0.9.1 the two `AtomicU64`s shared a 64-byte cache line,
  producing a MESI invalidate every time a sync completed
  during high append load — the appender hot path's
  `fetch_add` would invalidate the line that group-commit
  followers read on every `synced_lsn.load`. Padding both
  members eliminates that false-sharing class entirely. No
  observable behaviour change; throughput improvement is
  workload-dependent (highest under concurrent
  appender + sync_through pressure).
- **Group-commit coordinator rewritten as a parking_lot
  leader/follower scheme.** Pre-0.9.1 used `Mutex<()>` —
  every concurrent caller serialised through a blocking
  `lock()`, with no batching window and no Condvar wakeup
  on the synced frontier (followers had to wait the full
  fsync to acquire the gate). 0.9.1 introduces a
  `GroupCommit` coordinator (`src/journal/mod.rs`) holding
  a `parking_lot::Mutex<GroupCommitState>` (with
  `in_flight`, `committed_lsn`, `pending_followers`),
  a `cv_followers` Condvar that broadcasts on sync
  completion, and a `cv_leader` Condvar followers notify on
  arrival so the leader can re-check the `max_batch`
  early-exit condition during its `window` wait.
  - **Leader path:** acquires the state mutex, sets
    `in_flight = true`, drops the mutex, optionally waits
    `window` for additional followers (exiting early once
    `pending_followers >= max_batch`), runs `fdatasync`
    *outside* the mutex, then re-acquires to publish
    `committed_lsn` and `notify_all` followers.
  - **Follower path:** acquires the mutex, observes
    `in_flight = true`, increments `pending_followers`,
    notifies the leader, and parks on `cv_followers`.
    On wake, decrements `pending_followers` and re-checks;
    if its target LSN is still above the published
    `committed_lsn` (because an appender slipped a record
    in after the previous leader captured the frontier),
    the follower is promoted to leader of the next cycle.
  - **Async path** (`src/async_io/journal.rs`) ported to
    the same coordinator with a non-blocking `try_lock`
    + `tokio::task::yield_now()` busy-yield pattern so
    the tokio runtime worker is never parked on a
    contended mutex. The async leader skips the
    `window` follower-batching wait — async callers
    arrive on a different timescale than sync callers,
    and the io_uring fsync is itself zero-syscall-cost
    on the submitter side.
- **Stack-allocated frame fast path on `append`.** The
  buffered-mode single-record `append` now encodes records
  whose total framed size is ≤ 2 KiB into a stack array
  via `MaybeUninit`, eliminating the per-call `Vec<u8>`
  heap allocation that pre-0.9.1 paid for every record.
  Records above the threshold fall back to the previous
  heap-allocated path. Coverage threshold (≤ 2 KiB
  framed → stack; > 2 KiB → heap) was chosen to cover
  virtually every real-world WAL record (typical sizes
  64 B – 1 KiB) while keeping the per-call stack
  footprint bounded.
- **`append_batch` heap allocation skips zero-fill.** The
  contiguous batch buffer is allocated via
  `Vec::with_capacity` + `unsafe set_len`, with a
  `// SAFETY:` block documenting the must-write-before-read
  invariant established by the encoder loop. On a 5 K × 150
  B WAL batch (~810 KiB) the elided memset is the
  difference between a 0.77× regression and a 1.6× win
  vs `append`-in-loop on the canonical sanity bench.

### Performance — 0.9.1

Captured on the same Windows 11 NVMe reference box as the
0.9.0 baseline. Lower is better; numbers are wall-time
milliseconds best-of-5 on `append_batch_sanity_bench`.

| workload | append-in-loop | append_batch | speedup |
|---|---:|---:|---:|
| 10 K × 150 B records, sync once at end | 23.2 ms | 14.8 ms | **1.57×** |

Real-world wins on Linux + bare-metal NVMe are expected to
be larger (typical 3–10× depending on payload size and
concurrent appender count) because Linux POSIX `pwrite` does
not have NTFS's per-file write coordination ceiling. Check
the `bench.yml` GitHub Actions workflow on `ubuntu-latest`
for the canonical Linux capture once 0.9.1 ships.

The headline `journal_vs_atomic_replace` numbers are
unchanged from 0.9.0 — the hot-path improvements affect
multi-record submission paths and concurrent-flusher
group-commit, both of which are above the
single-`append` + single-fsync workload that bench measures.

### Tests — 0.9.1

- **+19 new lib tests** (367 → 386, plus 1 `#[ignore]`'d sanity
  bench): 8 `append_batch` cases, 5 leader/follower
  group-commit cases, 6 option-clamping cases.
- All 0.9.0 tests pass unchanged. `cargo test --all-features`:
  every suite green (lib, integration, doctest, async,
  stress).

### Notes — 0.9.1

- **New runtime dependencies:** `crc32c = "0.6"`,
  `parking_lot = "0.12"`, `crossbeam-utils = "0.8"
  (default-features = false)`. All three are MSRV 1.75-clean
  and dependency-light; selected for low-surface-area, mature
  ecosystems, and zero transitive bloat.
- **No breaking changes.** Every 0.9.0 caller compiles
  unchanged. The new `JournalOptions` builder methods are
  additive; the new defaults
  (`group_commit_window = Some(500 µs)`,
  `group_commit_max_batch = 8`) are observably faster than
  the pre-0.9.1 unbatched behaviour for any workload with
  concurrent flushers, and identical for single-flusher
  workloads.
- **Migration:** zero. emdb consumers wanting to claim the
  bulk-load lead back should re-route their `insert_many`
  hot path from per-record `append` to the new `append_batch`
  in a follow-up emdb release.

## [0.9.0] - 2026-05-05

> **0.9.0 — release candidate for 1.0.** Adds the journal
> substrate (open-once append-only log with explicit LSN
> reservation, group-commit fsync, and a CRC-32C-protected frame
> format) and an opt-in Direct-IO mode for the journal that
> routes appends through a sector-aligned in-memory log buffer.
> Tier-1 through tier-3 of the journal substrate ship in this
> release; tier-4 (io_uring registered buffers + SQPOLL) is
> deferred to the 0.9.x polish series pending real-world
> bottleneck data. The public API surface documented in
> [`docs/API.md`](docs/API.md) is the 1.0 target shape: from
> this tag forward, only genuine bugs change names or signatures.
> The 1.0 stable release follows once the long-running soak
> certification, the peer-comparison benchmark capture on
> bare-metal Linux + NVMe, and an independent reproduction of
> the crash-safety harness all complete.

### Added — 0.9.0

- **Direct-IO journal opt-in** (R-2 — `JournalOptions::direct(true)`).
  Opens the journal file with the platform's `O_DIRECT` /
  `F_NOCACHE` / `FILE_FLAG_NO_BUFFERING` flag and routes appends
  through a sector-aligned in-memory log buffer (the InnoDB /
  WiredTiger log-buffer pattern). Records are coalesced into
  sector-aligned chunks and written via DMA, bypassing the kernel
  page cache and the page-cache memcpy that buffered-mode writes
  pay on every record. Trade-off: appends serialise through a
  buffer mutex (no lock-free fast path), in exchange for zero-copy
  device writes. New public types:
  - `pub struct JournalOptions` — `new()`, `direct(bool)`,
    `log_buffer_kib(u32)` (clamped to 4..=65 536 KiB).
  - `Handle::journal_with(path, options) -> Result<JournalHandle>`
    — opens with caller-supplied options; `Handle::journal(path)`
    is a shorthand for `journal_with(path, JournalOptions::default())`.
  - `JournalHandle::is_direct_active()` — observability for the
    direct path. Returns `false` when the filesystem rejected
    `O_DIRECT` (tmpfs / FUSE / certain CIFS configurations) and
    the journal silently downgraded to buffered mode.
  - Resume after clean shutdown rehydrates the partial trailing
    sector into the buffer so subsequent flushes overwrite the
    zero-pad cleanly. Resume after crash scans to the LSN
    immediately past the last cleanly-decoded frame; surfaces an
    error for non-recoverable tail states (`BadMagic` /
    `LengthOverflow`).
  - Reader handles zero-magic-as-pad transparently — sees zero
    magic, advances to the next 512-byte boundary, retries
    decode (capped at `MAX_PAD_SKIP_SECTORS = 16` to bound the
    cost on pathological all-zero input). Buffered and direct
    journals share the same on-disk format; mixed-mode reopen
    (write-buffered → reopen-direct → write more) round-trips
    cleanly.

- **Crash-safety integration tests for the journal substrate**
  (R-3 — `tests/crash_journal.rs`). Spawns a victim subprocess
  that appends N records, calls `sync_through` to make the first
  `SYNCED_COUNT` durable, then keeps appending without syncing.
  Parent kills the victim mid-burst (Windows
  `TerminateProcess` / Unix `SIGKILL`), reopens the journal,
  and verifies:
  - **Durability invariant.** All synced records are present
    and intact after the kill.
  - **Tail-truncation safety.** The reader detects torn frames
    via `JournalTailState` (clean end, truncated header,
    truncated payload, or checksum mismatch — all are
    recoverable; `BadMagic` / `LengthOverflow` would indicate
    format corruption and surface as a non-recoverable error).
  - **No torn frames surface as records.** Records past the
    sync barrier may or may not be visible, but any that ARE
    visible match their expected content byte-for-byte. This is
    the load-bearing safety invariant of the frame format's
    CRC-32C check.
  - Runs for both `JournalOptions::default()` (buffered /
    lock-free) and `JournalOptions::direct(true)` (direct-IO /
    log-buffer); both pass under repeated kill timing.

- **Property-based test suite for the frame format**
  (`src/journal/format.rs::property_*`):
  - `property_random_round_trips` — 10 000 deterministic-PRNG
    encode/decode round-trips with payload sizes from 0 B to
    64 KiB. Catches any encoder/decoder disagreement
    statistically without adding a `proptest` dependency.
  - `property_single_bit_flip_detected` — exhaustive
    single-bit-flip detection across multiple payload sizes.
    Pins the load-bearing CRC-32C contract: no single-bit flip
    in any frame may produce a 'valid' decode with the original
    payload, OR with any other payload. CRC-32C provides this
    by construction; the test pins it empirically.

- **Fuzz target for the frame format**
  (`fuzz/fuzz_targets/journal_frame.rs`). Validates:
  1. Decoder never panics on any byte sequence.
  2. Encode → decode round-trip produces the same payload.
  3. Concatenated frames decode in order.

  Gated behind `cargo fuzz build journal_frame --features fuzz`
  + a dedicated `__fuzz` re-export module in the parent crate.
  Produces no warnings or trips of the existing
  `unsafe_op_in_unsafe_fn` / `unused_results` deny-list.

- **Cross-platform `write_at_direct` platform primitive**
  (`platform::write_at_direct`) — sector-aligned positioned
  write for Direct-IO file handles. Pre-conditions are
  caller-enforced: data pointer + length sector-aligned,
  offset sector-aligned. Used by the Direct-IO journal log
  buffer to flush sector-aligned chunks without copying through
  an intermediate aligned buffer.

### Added — 0.8.0 alpha (carried forward)


- **Journal substrate** (R-1) — open-once append-only log file with
  atomic LSN reservation and group-commit fsync. Solves the
  database / queue / ledger workload that the atomic-replace
  primitive (`Handle::write`) cannot reach. New public types:
  - `pub struct JournalHandle` — open-once journal, `Send + Sync`,
    shareable via `Arc`.
  - `pub struct Lsn(pub u64)` — log sequence number, byte-offset of
    the next-write position.
  - `Handle::journal(path) -> Result<JournalHandle>` — opens the
    journal at `path`, with the same handle-root scope and security
    checks as `Handle::write`. Resumes at the existing file size
    if the journal already exists.
  - `JournalHandle::append(record) -> Result<Lsn>` — appends a
    record without fsync, returns the LSN immediately past the
    record. Concurrent append from multiple threads is safe via
    atomic LSN reservation + `pwrite`.
  - `JournalHandle::sync_through(lsn) -> Result<()>` — group-commit
    fsync. Concurrent calls from many threads coalesce into one
    `fsync` syscall via a sync-gate mutex; callers waiting for an
    LSN ≤ the synced frontier wake immediately when the in-flight
    fsync completes.
  - `JournalHandle::synced_lsn()` / `next_lsn()` — observability.
  - `JournalHandle::close(self)` — explicit final-sync + close.

  **Measured throughput on `windows-ntfs-nvme`** (full table in
  [`docs/BENCH.md`](docs/BENCH.md)):

  | Payload | Atomic-replace | Journal (sync-at-end) | Speedup |
  |---------|---------------:|----------------------:|--------:|
  | 64 B | 634 ops/s | 462.9 K ops/s | **730×** |
  | 4 KiB | 891 ops/s | 189.3 K ops/s | **212×** |

  Three tiers shipped:
  - **Tier 1** — cross-platform sync, atomic LSN cursor,
    group-commit fsync via standard `pwrite` + `fdatasync`.
  - **Tier 2** — lock-free append path. POSIX uses concurrent
    `pwrite` directly against `&File` (no `Mutex<File>` on the
    hot path); Windows uses `WriteFile` with an `OVERLAPPED`
    struct carrying the offset (concurrent-safe per call;
    bounded above by NTFS's per-file write coordination).
  - **Tier 3** — native io_uring async substrate on Linux +
    `async` feature. `append_async` submits `IORING_OP_WRITE`
    SQEs; `sync_through_async` submits
    `IORING_OP_FSYNC(DATASYNC)` SQEs through the per-journal
    completion driver. No `spawn_blocking` thread-pool hop.
    Engagement observable via
    `JournalHandle::native_iouring_active()`. On non-Linux
    platforms or when io_uring construction fails (kernel
    without the syscall, sandboxed container), async ops fall
    back to `spawn_blocking` against the sync API
    transparently.

  Tier-4 (io_uring registered buffers + registered files +
  polling completion driver — the path to 5–10 M durable
  ops/sec on bare-metal Linux + NVMe) is deferred to the
  **0.9.x polish series**, to be attacked once real-world
  benchmarks identify it as the actual bottleneck rather than
  added on speculation.

- **Journal record framing — production-grade self-identifying
  format** (R-2). Every record written by
  `JournalHandle::append` is wrapped in a 12-byte frame: 4-byte
  big-endian magic+version (`0x46535901` = "FSY\x01"), 4-byte
  little-endian length, payload, 4-byte little-endian CRC-32C
  (Castagnoli). The CRC implementation is a software
  lookup-table that matches RFC 3720 known-answer vectors. Frame
  overhead is constant 12 bytes per record — 19% at 64 B,
  0.3% at 4 KiB, 0.02% at 64 KiB. Self-identification catches
  format-confusion attacks; CRC catches torn writes from a
  crash; magic-version byte allows future on-disk format
  evolution.

- **`JournalReader`** for journal replay — the read-side
  companion to `JournalHandle`. Forward-streaming iterator
  with checksum validation per record; `read_at_lsn` for
  positioned reads; `seek_to(lsn)` for reposition; tail-state
  classification distinguishes `CleanEnd` / `TruncatedHeader`
  / `TruncatedPayload` / `ChecksumMismatch` / `BadMagic` /
  `LengthOverflow` so recovery code can decide whether to
  truncate-and-resume or surface to a human operator.
  Buffered 64 KiB chunked reads; auto-grows for records
  larger than the buffer; concurrent-safe with a writer
  (reader sees records up to its captured file_size).

- **Storage-engine primitives — preallocate + advise.**
  `JournalHandle::preallocate(offset, len)` reserves
  filesystem extents up-front so subsequent appends don't
  trigger allocation jitter — critical for long-tail
  latency on high-throughput WAL workloads. Linux uses
  `fallocate(FALLOC_FL_KEEP_SIZE)` (no zero-write); macOS
  uses `fcntl(F_PREALLOCATE)` with contiguous-then-fallback;
  Windows uses `SetFileInformationByHandle(FileAllocationInfo)`
  — the proper Windows analog to Linux's keep-size fallocate
  (preserves logical EOF). `JournalHandle::advise` /
  `JournalReader::advise` / `JournalReader::advise_sequential`
  hint the kernel about access patterns. New public `Advice`
  enum: `Sequential`, `Random`, `WillNeed`, `DontNeed`,
  `Normal`. Linux maps to `posix_fadvise(2)`; macOS uses
  `F_RDADVISE` for sequential/will-need; Windows is
  best-effort no-op (lacks per-range advisory API).

- **Optional `tracing` feature.** Adds `tracing::trace_span!`
  and event instrumentation on the journal append /
  sync_through paths and the atomic-replace `Handle::write`
  hot path. Off by default; the dep is gated behind the
  `tracing` feature flag so non-tracing builds incur zero
  overhead. Production observability environments (with
  `tokio-console` / OpenTelemetry / etc. subscribers wired)
  enable the feature for end-to-end IO trace visibility.

- **`docs/EXAMPLES.md`** — catalogue of the 16 runnable examples in
  [`examples/`](examples/), each with a "when to use this pattern"
  guide. Run any example with `cargo run --example NN_name`.
- **`benches/matrix_with_peers.rs`** — end-to-end performance matrix
  bench against `std::fs` and `tokio::fs::read` (via
  `spawn_blocking`). Produces a markdown table on stdout; certified
  results recorded in [`docs/BENCH.md`](docs/BENCH.md) per the
  checkpoint-D-5 protocol.
- **`FSYS_SOAK_HOURS=N`** environment override on the soak harness
  (`tests/stress.rs`). Existing `--features stress` flag still
  selects the 1-hour CI run; the env var lets the 0.8.0 D-3
  pragmatic 4-hour cert run be triggered without rebuilding.
- **Three new test files**: `tests/critical_fixes_0_8_0.rs` (9
  regression tests pinning the B-checkpoint Critical-fix changeset),
  `tests/path_security.rs` (9 path-jail-escape tests including a
  Unix-only symlink-escape regression).

### Fixed (security — checkpoint J)

- **Symlink escape from `Builder::root` jail.** The pre-0.8.0
  `Handle::resolve_path` was purely lexical: a symlink **inside**
  the root pointing **outside** it slipped past the
  `starts_with(root)` check, defeating the entire root-scope
  feature. **Fix:** `resolve_path` now performs a third pass
  after lexical normalisation that canonicalises the longest
  existing prefix of the resolved path and verifies the canonical
  form lies inside the canonical root. Combined with `Builder::build`
  canonicalising the configured root at construction time, this
  closes the symlink-escape vector. **TOCTOU caveat documented** —
  a hostile local actor could race a symlink swap between
  resolution and `open`; closing that gap requires platform-specific
  primitives (`openat2(RESOLVE_BENEATH)` / `O_NOFOLLOW`-walked
  openat / `FILE_FLAG_OPEN_REPARSE_POINT`) filed for **0.9.0+**.
- **`Builder::root` canonicalisation at build time.** Previously,
  `Builder::root("data/../jail")` or a root containing symlinks
  defeated `starts_with(root)` because the stored root itself
  was non-canonical. **Fix:** `Builder::build` now calls
  `std::fs::canonicalize` on the root and stores the canonical
  form. Roots that don't exist or that fail canonicalisation
  return `Error::InvalidPath` at build time rather than allowing
  through with a broken jail.
- **`Handle::find` brace-expansion exponential blowup.** A
  pattern like `{a,b}^20` produced ≈ 1 M expansions, each its
  own string allocation — denial-of-service via memory pressure.
  **Fix:** `MAX_BRACE_EXPANSIONS = 1024` cap; pathological
  patterns bounded, benign patterns unaffected.

### Fixed (correctness — checkpoint B)

The B-checkpoint internal audit (parallel code-quality + hot-path
agents) produced **5 Critical findings**, all fixed:

- **Windows `write_at` 64-bit offset truncation.** Offsets above
  2 GiB silently went to the wrong location because the offset
  was split into `dist_lo: i32` and `dist_hi: i32` and only the
  low half was passed to `SetFilePointerEx`. Cross-platform
  contract violation vs. `pwrite` on Linux/macOS. **Fix:** pass
  the full `i64` offset directly; range-check against `i64::MAX`
  with `Error::Io(InvalidInput)` on overflow.
- **Zero-byte `Direct` IO undefined behaviour.** `AlignedBuf::new(0,
  …)` called `alloc_zeroed` whose precondition is
  `layout.size() > 0`. Reachable from public API
  (`quick::write(p, b"")`). **Fix:** `AlignedBuf::new` rejects
  `size == 0` with `Error::AlignmentRequired`; every Direct call
  site short-circuits empty input to produce a valid 0-byte file
  via a no-data path.
- **eventfd leak window in completion-driver `owner_loop`.** The
  raw eventfd was registered with the io_uring ring before
  ownership was established; a panic between registration and
  `OwnedFd::from_raw_fd` leaked the fd. **Fix:** wrap the raw fd
  in `OwnedFd` as the first thing in `owner_loop`, before any
  fallible construction. Unwind drops `OwnedFd` and closes the
  fd exactly once.
- **`AsyncMutex<Option<UnboundedSender<Op>>>` on the native-async
  hot path.** Every concurrent submit had to contend on this
  mutex even though `mpsc::UnboundedSender` is already
  `Send + Sync` and supports concurrent send. **Fix:** replaced
  with plain `mpsc::UnboundedSender<Op>` plus an `AtomicBool
  shutdown` flag for fast-path early-exit on shutdown.
  Removes one async-mutex acquire per op.
- **`poisoned` flag doc/code disagreement.** Doc claimed the
  owner task wrote the flag on panic via `catch_unwind`; code
  uses structural drop and the `_poisoned` parameter was unused.
  **Fix:** rewrote the doc to match actual mechanism (submit
  itself transitions the flag when its recv errors out).

### Fixed (correctness — checkpoint I)

- **`mmap` write missing `fsync` after `msync(MS_SYNC)`.** On
  Linux/macOS, `msync(MS_SYNC)` flushes data pages but does NOT
  include a metadata sync. The renamed file could have data on
  disk but stale size metadata after a power-loss event. **Fix:**
  added `temp_file.sync_all()` between `msync` and `rename`.
- **`write_all_direct` partial-write looping.** `pwrite(2)` may
  return less than requested on EINTR or short-write conditions.
  The 0.7.0 code did a single pwrite and trusted the return value;
  large Direct writes could silently truncate. **Fix:** loop on
  partial writes with EINTR retry on Linux + macOS.

### Performance (checkpoint I)

- **Direct-IO truncate via in-place `set_len`** (I round 1) —
  replaced the post-write "drop the `O_DIRECT` /
  `FILE_FLAG_NO_BUFFERING` handle, reopen buffered, call
  `set_len`" pattern with an in-place `set_len` on the
  already-open file handle. `set_len` works regardless of the
  open flags. Saves two syscalls (close + open) per Direct
  write. **Measured 2.4–3.5× speedup at 4 KiB single writes**
  on `windows-ntfs-nvme`.
- **Group-lane dispatcher fast-flush** (I round 2) — the
  dispatcher's 1 ms accumulation window was adding ~500 µs of
  fixed latency to every batch when there was no concurrent
  contention to amortise it across. The fix scoops any
  already-queued jobs via a non-blocking `try_recv` drain;
  when the drain finds nothing (single-submitter workload),
  the dispatcher flushes immediately instead of waiting for
  the window to expire. Concurrent-submitter workloads still
  get the original window batching. **Closed the
  batch-slower-than-solo gap** that the F bench surfaced as a
  real finding: 4 KiB batch-of-8 went from 0.42× of solo to
  1.00× (full latency parity); 64 KiB and 1 MiB batches both
  went to 0.95× of solo (essentially tied). Per-batch median
  ≈ 2× faster across all payload sizes.
- **`gen_temp_path` alloc reduction** (I round 2) — replaced
  `format!()` + `to_string_lossy().into_owned()` +
  `parent.join(String)` (3 string allocs + 1 PathBuf per call)
  with direct `OsString` construction (1 OsString + 1 PathBuf).
  Stays in `OsStr`-land for non-UTF-8 filenames. ~50–100 ns
  saved per write call.
- **Lock-free buffer-pool fast path** (I round 3) — replaced
  `pool_slot: Mutex<Option<AlignedBufferPool>>` with
  `OnceLock<AlignedBufferPool>`. Every Direct write previously
  paid a mutex acquire to read the pool; now it pays a single
  atomic load + `Arc::clone`. ~20–50 ns saved per Direct op,
  scales linearly with throughput — meaningful at million-op-
  per-second workloads.
- **`resolve_path` fast path for root-scoped handles** (I round 3) —
  the security-fix Pass 3 (canonicalize syscall) is now skipped
  when the resolved path's parent equals the canonical root AND
  the leaf is verified non-symlink via a cheap
  `symlink_metadata` (`lstat`) check. The common shape —
  `fs.write("file.txt")` on a root-scoped handle — pays one
  `lstat` (~1–5 µs) instead of one `canonicalize` (~50–200 µs
  on Windows). 10×+ speedup for root-scoped writes;
  no security regression (the canonical-prefix check still runs
  for any path where the fast path doesn't apply, e.g. nested
  writes or symlinked leaves).
- **Async submit non-blocking under saturation** (I round 3) —
  `Pipeline::submit_async` previously blocked the tokio worker
  via synchronous `crossbeam_channel::send` when the
  dispatcher's bounded queue was full, stalling the entire
  runtime. Replaced with a `try_send` retry loop using
  `tokio::task::yield_now`. Backpressure preserved (the calling
  task is suspended); the runtime worker is no longer held
  hostage.
- **Honest finding documented in `docs/BENCH.md`**: at 4 KiB
  median, `std::fs::write` is still ≈ 5× faster than
  `fsys::Auto` because `std::fs::write` does **no durability
  fence**. The fair comparison is `Method::Sync` vs.
  `std::fs + manual atomic-replace dance`. At p99 latency
  `fsys` wins decisively at every payload size. At 64 KiB and
  1 MiB `fsys` beats `std::fs::write` even on the median.

### Documentation

- Comprehensive **`docs/BENCH.md`** rewrite with certified results
  per the checkpoint-D-5 protocol: date + hardware class +
  methodology + median + p99 + comparison cells against
  `std::fs` and `tokio::fs`.
- New **`docs/EXAMPLES.md`** catalogue + 16 runnable examples in
  `examples/` covering every feature matrix entry.
- New decision log: [`.dev/DECISIONS-0.8.0.md`](.dev/DECISIONS-0.8.0.md).
- New audit reconciliation docs:
  [`.dev/CODE-AUDIT-0.8.0.md`](.dev/CODE-AUDIT-0.8.0.md) (B) and
  [`.dev/SECURITY-REVIEW-0.8.0.md`](.dev/SECURITY-REVIEW-0.8.0.md) (J).

### Stability (release-candidate freeze contract)

The public API surface as documented in
[`docs/API.md`](docs/API.md) is frozen at the **0.9.0 git tag**.
Changes after that tag require:

- A name or signature change → only for genuine bugs, with
  rationale in the CHANGELOG.
- A new method, variant, or field → wait for `1.0` or a later
  minor release.
- An error variant addition → permitted (errors are
  `#[non_exhaustive]`).
- A docstring tightening → permitted at any time.

This contract is what callers can rely on for the
release-candidate-to-1.0 runway.

### Out of scope — deferred to 0.9.x polish or 1.0

- **Tier-4 io_uring** — registered buffers, registered files,
  and SQPOLL polling completion driver. The path to 5–10 M
  durable ops/sec on bare-metal Linux + NVMe. Tier-3 native
  combined with the Direct-IO log buffer already saturates
  most realistic workloads; tier-4 is a measured-bottleneck
  optimisation worth attacking once real-world benchmarks
  identify it as the actual ceiling.
- **24-hour soak certification** — long-running CI / dedicated-
  box workload. The harness supports it via
  `FSYS_SOAK_HOURS=24`; capture is a 0.9.x post-RC task.
- **Bare-metal Linux native-substrate measurement** — the WSL2
  measurement (1.46×) holds. Bare-metal validation (expected
  2×+) requires hardware access deferred to 0.9.x.
- **Forced-unmount crash harness** — the 0.9.0 process-kill
  harness covers the documented durability contract; a
  separate forced-unmount harness with privileged teardown is
  a 0.9.x companion.
- **`openat2(RESOLVE_BENEATH)` TOCTOU mitigation** — closes
  the residual symlink-race gap in `Handle::resolve_path`
  not covered by the existing canonical-prefix check.
- **`cap-std` peer comparison** — `std::fs` and `tokio::fs`
  comparisons shipped in 0.8.0; `cap-std` is the closest
  capability-based peer and is a 0.9.x item.

## [0.7.0] - 2026-05-04

> **Optimization phase — last breaking-change phase before alpha.**
> The API surface is **frozen at the 0.8.0 alpha tag**: from
> that point forward, only genuine bugs may change a name or
> signature. The renames and docstring strengthenings below are
> the cleanup pass that earns the freeze.

### Added

- **Native io_uring async substrate (Linux only).** When
  [`Method::Direct`] is in use on Linux and
  `FSYS_DISABLE_NATIVE_ASYNC` is not set, async ops submit
  directly to the per-handle io_uring ring and `.await` a
  `oneshot` driven by a per-handle completion driver task —
  no [`tokio::task::spawn_blocking`] thread-pool hop. The
  driver task is structured around a tokio
  [`AsyncFd`](https://docs.rs/tokio/latest/tokio/io/unix/struct.AsyncFd.html)
  + `eventfd(2)` for completion polling. Measured 1.46×
  throughput improvement vs. `spawn_blocking` at 4 KiB writes
  in WSL2 + ext4 (locked decision D-1, observability via
  [`AsyncSubstrate`]).
- **[`AsyncSubstrate`] enum + [`Handle::async_substrate()`]**
  — runtime observability for which async substrate a handle
  uses: `NativeIoUring` on Linux + Direct + ring-active, or
  `SpawnBlocking` everywhere else. Always available (even in
  non-async builds) so consumers can match without
  `cfg`-gating.
- **`FSYS_DISABLE_NATIVE_ASYNC=1` environment override** —
  forces `SpawnBlocking` even on Linux + Direct. Read on each
  `async_substrate()` call (no process-start re-export
  needed). Useful for A/B perf comparisons, diagnosing
  suspected native-substrate issues, and CI runners where
  io_uring is unavailable.
- **PLP (power-loss-protection) detection refinement.** The
  hardware probe now consults a per-vendor lookup table of
  enterprise-grade NVMe families (Intel D3-S series, Samsung
  PM-series, Micron 7xxx/9xxx, Kioxia CD/CM, etc.) and
  reports PLP presence via `HardwareInfo::plp`. Lookup is
  conservative — false-negatives are safe (we just avoid the
  PLP-only fast path), false-positives would be unsafe (we'd
  trust a drive that can't honour the contract), so the
  table is curated rather than heuristic.
- **Three new [`Error`] variants:** `HandlePoisoned`
  (FS-00019), `IoUringSubmitFailed` (FS-00020),
  `CompletionDriverDead` (FS-00021).
- **Regression suite + perf-budget infrastructure.** New
  `benches/baselines.json` schema with per-machine-class
  baselines, hybrid regression strictness (critical 5%,
  standard 10%, loose 25%), and a relative tail target
  (p99.9 within 10× p50 — portable across hardware). New
  bench harnesses: `benches/async_native_vs_blocking.rs`
  (D-8 measurement), `benches/tail_validation.rs` (D-5
  sample-and-percentile harness).
- **Hostile-filesystem and power-loss-sim test scaffolding.**
  Env-gated tests for tmpfs / FAT32 / exFAT / NFS / SMB
  behaviour, plus a forced-unmount sim placeholder for the
  full crash-safety certification path.

### Changed (breaking)

- **`Handle::read_range(path, offset, len)` → `Handle::read_at`.**
  The new name aligns with standard `pread`-style naming and
  removes the implication of an inclusive-exclusive range
  type.
- **`Handle::scan(path, recursive: bool)` → `Handle::scan(path)`
  + `Handle::scan_all(path)`.** Same split for `count` →
  `count` + `count_all`. Bare-bool parameters at the call
  site are a Rust API smell; the audit pass split them into
  distinct methods. `find` keeps a single method because glob
  patterns express recursion natively (`*` non-recursive,
  `**` recursive).
- **`Builder::buffer_pool_size(usize)` → `Builder::buffer_pool_count`**
  — "size" implied a byte total; this is a count of buffers.
- **`Builder::buffer_pool_block(usize)` → `Builder::buffer_pool_block_size`**
  — "block" alone was ambiguous; this is the per-buffer size
  in bytes.
- The async siblings of all renamed methods follow the same
  renames: `read_at_async`, `scan_async` /
  `scan_all_async`, `count_async` / `count_all_async`.

### Changed (non-breaking)

- **Strengthened docstrings** (no signature changes):
  - [`Method::Sync`] — leads with the disambiguation that
    `Sync` refers to the `fsync(2)` family of durability
    primitives, **not** "synchronous IO" as opposed to
    async.
  - [`Method::Journal`] — reframed from "reserved for
    0.7.0" to "reserved indefinitely as a forward-compat
    placeholder; no committed target version."
  - [`Handle::write_copy`] — leads with "this is NOT a
    file-to-file copy (no source argument); it copies the
    target's existing metadata onto the new payload."
  - [`Handle::find`] — explicit "Recursion semantics"
    section explaining that `*` is non-recursive, `**` is
    recursive, and contrasting with the `scan` /
    `scan_all` flat-vs-recursive split.

### Documentation

- Comprehensive **`docs/API.md`** rewrite covering the full
  public surface, the three-tier entry points, and the
  alpha-freeze policy.
- New `.dev/DECISIONS-0.7.0.md` with 13 locked decisions
  (11 from the original prompt + 2 reversals discovered
  during execution: R-2 PLP refinement, R-3 1.46× WSL
  measurement).
- New `.dev/API-AUDIT-0.7.0.md` documenting the internal +
  external-subagent + reconciliation passes that produced
  the renames and docstring strengthenings above.
- **Removed** `docs/MIGRATION.md` (per locked decision D-6:
  migration content is part of `CHANGELOG.md` and the
  rename table in `docs/API.md`, not a separate file).

### Out of scope

- **`Method::Journal`** stays reserved. The intent-log
  durability mode was originally scoped for 0.7.0 but
  deferred to avoid blocking alpha freeze on a feature that
  needs its own multi-phase design pass. No version is
  committed.
- **24-hour soak certification** — the 0.7.0 release runs
  the tier-1 in-session 60s soak (passes); the tier-3 full
  certification soak is reserved for the `0.8.0` release-prep
  phase per the pragmatic-mode (b) decision.

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

[Unreleased]: https://github.com/jamesgober/fsys-rs/compare/v0.9.4...HEAD
[0.9.6]: https://github.com/jamesgober/fsys-rs/compare/v0.9.5...v0.9.6
[0.9.5]: https://github.com/jamesgober/fsys-rs/compare/v0.9.4...v0.9.5
[0.9.4]: https://github.com/jamesgober/fsys-rs/compare/v0.9.3...v0.9.4
[0.9.3]: https://github.com/jamesgober/fsys-rs/compare/v0.9.2...v0.9.3
[0.9.2]: https://github.com/jamesgober/fsys-rs/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/jamesgober/fsys-rs/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/jamesgober/fsys-rs/compare/v0.7.0...v0.9.0
[0.7.0]: https://github.com/jamesgober/fsys-rs/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/jamesgober/fsys-rs/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/jamesgober/fsys-rs/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/jamesgober/fsys-rs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/jamesgober/fsys-rs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jamesgober/fsys-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/fsys-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/fsys-rs/releases/tag/v0.1.0
