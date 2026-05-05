# Changelog

All notable changes to `fsys` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[Unreleased]: https://github.com/jamesgober/fsys-rs/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/jamesgober/fsys-rs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jamesgober/fsys-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/fsys-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/fsys-rs/releases/tag/v0.1.0
