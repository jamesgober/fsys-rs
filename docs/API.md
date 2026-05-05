<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  API DOCS
</h1>

> **Coverage.** This document describes every public item in
> the `fsys` crate as of `0.9.0`. The API is treated as
> **frozen at the 0.9.0 release-candidate tag** — only genuine
> bugs may change a name or signature after that point. The
> 1.0 stable tag inherits this surface verbatim once
> real-world testing confirms the API decisions. If you are
> reading this against `master`, the rendered docs at
> [docs.rs/fsys](https://docs.rs/fsys) are the source of truth.

---

## Table of contents

1. [Surface at a glance](#surface-at-a-glance)
2. [Three-tier entry points](#three-tier-entry-points)
3. [`Handle` — primary type](#handle--primary-type)
4. [`Method` — durability strategy](#method--durability-strategy)
5. [`Mode` — environment profile](#mode--environment-profile)
6. [`Builder` — advanced configuration](#builder--advanced-configuration)
7. [`Batch` — group-lane writes](#batch--group-lane-writes)
8. [Async API (`async` feature)](#async-api-async-feature)
9. [`AsyncSubstrate` — runtime substrate observability](#asyncsubstrate--runtime-substrate-observability)
10. [`Error` and `Result`](#error-and-result)
11. [Hardware / OS info accessors](#hardware--os-info-accessors)
12. [Path utilities](#path-utilities)
13. [Constants and primitives](#constants-and-primitives)
14. [Stability + breaking-change policy](#stability--breaking-change-policy)
15. [Journal substrate (`JournalHandle`, `JournalReader`, `JournalOptions`)](#journal-substrate)
16. [API changes in 0.9.0](#api-changes-in-090)

---

## Surface at a glance

`fsys` exposes **one primary type** (`Handle`), **one
configuration builder** (`Builder`), **one durability enum**
(`Method`), and a small set of supporting types. There are
three layers of API ergonomics:

| Layer | Use when | Example |
|---|---|---|
| `quick::*` | One-shot ops, no handle reuse | `fsys::quick::write("/p", b"x")?` |
| `builder().build()` | Default handle for the process | `let fs = builder().build()?;` |
| `Builder` chain | Custom method, root, pool sizing, etc. | `builder().method(Method::Direct).root("/data").build()?` |

All three paths converge on the same internal machinery; the
choice is purely about ergonomics and configuration depth.

```rust
use fsys::{builder, Method};

// Layer 1 — one-shot.
fsys::quick::write("/tmp/note.txt", b"hello")?;

// Layer 2 — process-wide handle.
let fs = builder().build()?;
fs.write("/tmp/note.txt", b"hello")?;

// Layer 3 — fully configured handle.
let fs = builder()
    .method(Method::Direct)
    .root("/data")
    .buffer_pool_count(128)
    .buffer_pool_block_size(65_536)
    .build()?;
# Ok::<(), fsys::Error>(())
```

---

## Three-tier entry points

### `fsys::builder()`

Returns a fresh [`Builder`](#builder--advanced-configuration)
with default settings. The most common path.

### `fsys::quick`

A module of free functions for one-shot operations. Each call
creates a default-method handle, performs the op, and drops the
handle. Use only for genuinely-one-shot work — for any program
that does more than a single op, hold a long-lived `Handle`.

| Function | Equivalent to |
|---|---|
| `quick::read(path)` | `builder().build()?.read(path)` |
| `quick::write(path, data)` | `builder().build()?.write(path, data)` |
| `quick::append(path, data)` | `builder().build()?.append(path, data)` |
| `quick::write_async(path, data)` | async variant (feature `async`) |
| `quick::read_async(path)` | async variant (feature `async`) |

### `fsys::with(method)` / `fsys::new()`

Shortcuts onto `Builder::new()`:
- `with(method)` — `Builder::new().method(method)`.
- `new()` — `Builder::new()` (and `Builder::default()`).

---

## `Handle` — primary type

A `Handle` owns the resolved method, root, mode, sector size,
pipeline, buffer-pool slot, io_uring slot (Linux), and NVMe
passthrough slot (Linux + Windows). It is `Send + Sync` and
`Clone` (clones share underlying resources via `Arc`).

### File CRUD

| Method | Purpose |
|---|---|
| `write(path, data)` | Atomic-replace write; durable on return. |
| `write_copy(path, data)` | Atomic-replace write **preserving the existing target's metadata** (mode/ACLs/timestamps). *Not* a file-to-file copy — see [`std::fs::copy`] for that. |
| `append(path, data)` | Append to an existing file (creates if missing). Not individually flushed; call `Handle::sync` for batched durability. |
| `read(path)` | Read full file contents into a `Vec<u8>`. |
| `read_at(path, offset, len)` | Read `len` bytes from `offset`. (Renamed from `read_range` in 0.7.0; see [API changes](#api-changes-in-070).) |
| `delete(path)` | Unlink a file. |
| `exists(path)` | `bool` for path existence. |
| `meta(path)` | `FileMeta` for a path (size, kind, times, mode). |
| `is_file(path)` | `bool` — is the path a regular file? |
| `is_dir(path)` | `bool` — is the path a directory? |

### Directory CRUD

| Method | Purpose |
|---|---|
| `mkdir(path)` | Create a directory (fails if parent missing). |
| `mkdir_all(path)` | Create directory and parents. |
| `rmdir(path)` | Remove an empty directory. |
| `rmdir_all(path)` | Remove a directory tree recursively. |
| `list(path)` | Immediate children as `Vec<DirEntry>`. |
| `scan(path)` | Like `list`, but returns `DirEntry` with `is_file` / `is_dir` populated. **Non-recursive.** |
| `scan_all(path)` | Recursive variant of `scan`. |
| `count(path)` | Count of regular files in `path`. **Non-recursive.** |
| `count_all(path)` | Recursive variant of `count`. |
| `find(path, pattern)` | Glob match within `path`. **Recursion is encoded in the pattern itself** — `*.log` is non-recursive, `**/*.log` is recursive. |

The `scan` / `scan_all` and `count` / `count_all` split (instead
of a `recursive: bool` parameter) is intentional. `find` is the
exception because glob patterns express recursion natively via
`**`.

### Observability

| Method | Returns |
|---|---|
| `active_method()` | The concrete `Method` in use after `Auto` resolution and any fallbacks (never `Method::Auto`). |
| `active_durability_primitive()` | A static string naming the OS primitive: `"fsync"`, `"fdatasync"`, `"f_fullfsync"`, `"flushfilebuffers"`, `"o_direct+fdatasync"`, `"io_uring"`, etc. Match against the constants in [`fsys::primitive`](#constants-and-primitives) to avoid typos. |
| `async_substrate()` | Which substrate the async layer uses. See [`AsyncSubstrate`](#asyncsubstrate--runtime-substrate-observability). |
| `mode()` | The resolved [`Mode`](#mode--environment-profile). |
| `sector_size()` | Probed logical sector size in bytes. |

### Sync

| Method | Purpose |
|---|---|
| `sync(path)` | Force-flush durability for `path` (used after a series of `append` calls). |

### Batch

| Method | Purpose |
|---|---|
| `write_batch(ops)` | Submit a vector of writes through the per-handle group-lane dispatcher. |
| `delete_batch(paths)` | Delete a vector of paths through the dispatcher. |
| `sync_batch(paths)` | Force-flush a batch of paths. |

The dispatcher is a single thread spawned lazily on the first
batch op; it serialises durability within the lane and amortises
syscall overhead across a flush window. See
[`docs/PERFORMANCE.md`](PERFORMANCE.md) for tuning.

---

## `Method` — durability strategy

```rust
pub enum Method {
    Sync,    // fsync(2) / F_FULLFSYNC / FlushFileBuffers
    Data,    // fdatasync(2) / fallback to Sync elsewhere
    Direct,  // O_DIRECT + io_uring (Linux) / F_NOCACHE+F_FULLFSYNC (macOS) / FILE_FLAG_NO_BUFFERING (Windows)
    Mmap,    // mmap + msync(MS_SYNC) / MapViewOfFile + FlushViewOfFile
    Journal, // RESERVED — no committed target version
    Auto,    // Hardware-aware selection
}
```

### Naming caveat

`Method::Sync` refers to the **`fsync(2)` family of durability
primitives**, not "synchronous IO" as opposed to async. All
durability methods are usable from both the sync and async
APIs. For async-vs-blocking selection, see
[`Handle::async_substrate`](#asyncsubstrate--runtime-substrate-observability).

### `Method::Journal` is reserved

`Method::Journal` is a **forward-compatibility placeholder**.
Building a handle with `Method::Journal` returns
[`Error::UnsupportedMethod`](#error-and-result). No version is
committed for the real backend; do not depend on it landing in
any specific release.

### `Auto` resolution ladder

`Method::Auto` consults the cached hardware probe
([`fsys::hardware::info`](#hardware--os-info-accessors)) and
picks the fastest method that is safe on the current hardware
and OS. The resolved concrete method is visible via
`Handle::active_method()` (which never returns `Auto`). See the
table in `Method::Auto`'s rustdoc for the full matrix.

---

## `Mode` — environment profile

```rust
pub enum Mode {
    Auto, // Resolved from FSYS_MODE / RUST_ENV
    Dev,  // Development defaults
    Prod, // Production defaults
}
```

`Mode::Auto` checks `FSYS_MODE` then `RUST_ENV`. The resolved
mode is visible via `Handle::mode()`.

> **Known ergonomic wart.** `Mode::Dev` / `Mode::Prod` collide
> with common downstream enum names. We have not renamed the
> variants for the alpha freeze (the cost-to-benefit of a wide
> refactor is poor). Fully-qualified usage
> (`fsys::Mode::Dev`) avoids the collision.

---

## `Builder` — advanced configuration

```rust
let fs = fsys::builder()
    .method(Method::Direct)
    .root("/data")
    .mode(Mode::Prod)
    .batch_window_ms(2)
    .batch_size_max(256)
    .batch_queue_max(2048)
    .buffer_pool_count(128)
    .buffer_pool_block_size(65_536)
    .io_uring_queue_depth(256)
    .build()?;
```

| Knob | Default | Notes |
|---|---|---|
| `method(Method)` | `Auto` | Durability strategy. |
| `root(P)` | `None` | Path-scope enforcement. Paths that escape the root are rejected with `Error::InvalidPath`. |
| `mode(Mode)` | `Auto` | Dev/Prod profile. |
| `batch_window_ms(u64)` | `1` | Group-lane time threshold. |
| `batch_size_max(usize)` | `128` | Group-lane count threshold. |
| `batch_queue_max(usize)` | `1024` | Group-lane queue capacity. |
| `buffer_pool_count(usize)` | `64` | Number of aligned buffers in the per-handle pool. *(Renamed from `buffer_pool_size` in 0.7.0.)* |
| `buffer_pool_block_size(usize)` | `4096` | Per-buffer size in bytes. *(Renamed from `buffer_pool_block` in 0.7.0.)* |
| `io_uring_queue_depth(u32)` | `128` | Linux io_uring SQ depth. |

---

## `Batch` — group-lane writes

`Batch` is the type returned by `Handle::batch()` for the
fluent batch-builder ergonomics. Operations accumulate via
`.write(path, data)` / `.delete(path)` / `.sync(path)` and
flush on `.commit()`.

For programmatic batch construction, prefer
`Handle::write_batch(Vec<BatchOp>)` directly.

`BatchError` is the error type returned by partial-failure
batches: it carries per-op error information without aborting
the rest of the batch.

---

## Journal substrate

For high-throughput durable writes (database WAL, queue persistence, ledger append), use the **journal substrate** — open-once log file, atomic LSN reservation, group-commit fsync. Shipped in 0.9.0.

### Public types

```rust
pub struct JournalHandle { /* opaque */ }
pub struct Lsn(pub u64);
```

`JournalHandle` is `Send + Sync`; share via `Arc<JournalHandle>`. `Lsn` is the byte-offset of the *next* write position in the journal — monotonic per-handle, with transparent ordering (`Lsn(100) < Lsn(200)` ⟺ first record was appended before the second).

### API

| Method | Purpose |
|---|---|
| `Handle::journal(path) -> Result<JournalHandle>` | Opens the journal at `path`. Resumes at the existing file size if the file exists; creates if not. |
| `JournalHandle::append(record) -> Result<Lsn>` | Appends a record (NO fsync). Returns the LSN immediately past the record. Concurrent-safe across threads. |
| `JournalHandle::sync_through(lsn) -> Result<()>` | Group-commit fsync: forces all bytes up to `lsn` to stable storage. Concurrent calls coalesce into one fsync syscall. |
| `JournalHandle::synced_lsn() -> Lsn` | Highest LSN known durable. Observability. |
| `JournalHandle::next_lsn() -> Lsn` | Next LSN to be assigned by `append`. Snapshot point. |
| `JournalHandle::close(self) -> Result<()>` | Final sync + close (consumes self). Drop is the implicit best-effort close. |
| `JournalHandle::append_async(self: Arc<Self>, record: Vec<u8>) -> Result<Lsn>` | Async sibling. Requires `async` feature + tokio runtime. |
| `JournalHandle::sync_through_async(self: Arc<Self>, lsn: Lsn) -> Result<()>` | Async sibling for group-commit. |

### When to use the journal

Use the journal when:
- You're building a **database WAL**, **persistent queue**, **event log**, or **ledger** — anywhere the natural unit is "stream of records" not "single-file replacement."
- You need **commit-LSN semantics**: durability is "everything up to LSN X is on disk," not "every individual write fsynced."
- You want **group-commit** — N records amortised across one fsync.

Use [`Handle::write`](#handle--primary-type) instead when:
- You need **atomic-replace semantics**: the file is either entirely the old payload or entirely the new payload at every observable point.
- You're updating individual files (config, manifests).
- Throughput is not the dominant concern.

### Throughput

Measured in [`docs/BENCH.md`](BENCH.md) on `windows-ntfs-nvme`:

| Payload | Atomic-replace | Journal (sync-at-end) | Speedup |
|---------|---------------:|----------------------:|--------:|
| 64 B | 634 ops/s | 462.9 K ops/s | **730×** |
| 4 KiB | 891 ops/s | 189.3 K ops/s | **212×** |

Bare-metal Linux + NVMe is expected to push the journal substrate to 1–3 M ops/sec on tier-2 (current) and 5–10 M ops/sec on tier-3 (Linux io_uring SQE batching + registered buffers, filed for 0.9.0).

### LSN semantics

- `append(record)` returns `Lsn(start_offset + record.len())` — the position just past the record.
- `sync_through(lsn)` blocks until every byte from offset 0 through `lsn.0 - 1` is on stable storage.
- `Lsn::ZERO` (= `Lsn(0)`) is the start-of-journal sentinel. `sync_through(Lsn::ZERO)` is a no-op.
- LSNs are monotonic per-handle — they reset only when the underlying file is truncated or recreated.

### Concurrency

- **Concurrent appends** — multiple threads can call `append` against the same `Arc<JournalHandle>`. The append path is lock-free (atomic LSN reservation + concurrent-safe `pwrite` per POSIX). On Linux + ext4/xfs, multi-thread aggregate throughput scales near-linearly with thread count up to the storage queue depth. On Windows + NTFS, the OS serialises writes to a single fd at the kernel-driver level — multi-thread aggregate is capped by NTFS's per-file write throughput (this is an OS limitation, not an fsys limitation).
- **Concurrent `sync_through`** — group-commit. Many threads calling `sync_through` simultaneously coalesce into one fsync syscall via an internal sync-gate.

### Tiering roadmap

| Tier | Status | Implementation |
|------|--------|----------------|
| Tier 1 | **Shipped 0.9.0** | Cross-platform synchronous core: atomic LSN reservation via `AtomicU64::fetch_add`, positioned writes via `pwrite` (POSIX) / `WriteFile` + `OVERLAPPED` (Windows), durability via `fdatasync` / `fsync` / `FlushFileBuffers`. Group-commit fsync coalesces concurrent `sync_through` callers behind a single mutex-gated syscall. |
| Tier 2 | **Shipped 0.9.0** | Lock-free append. POSIX uses concurrent `pwrite` directly against `&File` — no mutex on the hot path; per-POSIX, concurrent `pwrite` calls to distinct offsets are atomic per call. Windows uses `WriteFile` with an `OVERLAPPED` struct carrying the offset (concurrent-safe at the API level; OS-bounded by NTFS's per-file write coordination). |
| Tier 3 | **Shipped 0.9.0** | Native io_uring asynchronous substrate on Linux when the `async` feature is enabled. `append_async` submits `IORING_OP_WRITE` SQEs; `sync_through_async` submits `IORING_OP_FSYNC(DATASYNC)` SQEs. The calling tokio task awaits a `oneshot` driven by the journal's per-handle completion driver, eliminating the `spawn_blocking` thread-pool hop. Engagement is observable via [`JournalHandle::native_iouring_active`]. On non-Linux platforms, on Linux without the `async` feature, or when `io_uring_setup(2)` fails, async ops fall back to `spawn_blocking` against the synchronous tier. |
| Tier 4 | **0.9.x polish** | io_uring registered buffers (`io_uring_register_buffers` — no per-op kernel-side buffer pinning) + registered files (`io_uring_register_files` — no per-op fd-table lookup) + SQPOLL kernel polling thread (no syscall in the steady-state hot path). Target: 5–10 M durable ops/sec on bare-metal Linux + NVMe. Deferred from 0.9.0 RC because the tier-3 native substrate combined with the Direct-IO log buffer already saturates most realistic workloads; tier-4 is a measured-bottleneck optimisation worth attacking once real-world benchmarks identify it as the actual ceiling. |

### Direct-IO journal mode (0.9.0)

When `JournalOptions::direct(true)` is active, the journal opens the underlying file with `O_DIRECT` (Linux) / `F_NOCACHE` (macOS) / `FILE_FLAG_NO_BUFFERING` (Windows) and routes appends through a sector-aligned in-memory log buffer. This is the architecture that database storage engines use for their write-ahead log — most notably InnoDB (MySQL) and the WiredTiger journal (MongoDB) — to bypass the kernel page cache and write directly into device DMA.

**Architectural trade-off.** The lock-free hot path of the buffered tiers is replaced by a single mutex protecting the log buffer; appenders serialise through that mutex while their frames are copied into the shared buffer. In exchange, the journal gains:

- Zero memory copy from user space into the page cache. The frame is encoded into the log buffer once; the kernel writes it directly to the block device via DMA.
- Predictable tail latency. The page cache's writeback policy can introduce jitter on buffered writes; Direct IO removes that source of non-determinism.
- Reduced page-cache pressure. High-throughput WAL workloads otherwise compete for cache pages with the rest of the system.

**When to enable it.** The default (`direct(false)`) is the right choice for general-purpose use — the lock-free path scales linearly with thread count and the kernel page cache absorbs bursty writes well. Direct-IO mode is the right choice when:

- The workload is a database / queue / ledger WAL whose throughput is gated by sustained sequential append.
- Tail latency matters and the page cache's writeback jitter is observable in measurements.
- The system is otherwise memory-pressured and the WAL should not compete for page-cache space.

**Flush semantics.** The log buffer flushes to disk in two situations: (1) when the buffer fills, the entire buffer is written via a single sector-aligned positioned write and the buffer is reset for the next round; (2) when `sync_through` is called with unflushed records, the records plus zero-pad to the next sector boundary are written but the buffer's logical position is not advanced — subsequent appends continue filling from where they were, and the next buffer-full flush overwrites the prior partial-sector pad with real record bytes.

**Crash safety.** Resume after a crash scans the existing file forward from offset zero, finds the LSN immediately past the last cleanly-decoded frame, and reseats the log buffer's flush position at the largest sector boundary at or before that LSN. The partial trailing sector is rehydrated into the buffer's first sector so subsequent flushes overwrite the existing on-disk zero-pad cleanly without destroying records. Non-recoverable tail states (`BadMagic`, `LengthOverflow`) surface as an `Error::Io(InvalidData)` from `Handle::journal_with` rather than auto-truncating past suspect data.

---

## Async API (`async` feature)

Every sync `Handle` method has an async sibling with the same
name plus an `_async` suffix. The async layer requires a
running tokio runtime and is gated behind the `async` Cargo
feature.

```rust
use std::sync::Arc;
use fsys::builder;

#[tokio::main]
async fn main() -> fsys::Result<()> {
    let fs = Arc::new(builder().build()?);
    fs.clone().write_async("/tmp/note", b"hello".to_vec()).await?;
    let bytes = fs.clone().read_async("/tmp/note").await?;
    Ok(())
}
```

### Async method coverage

All sync methods have async siblings:
- File: `write_async`, `write_copy_async`, `append_async`,
  `read_async`, `read_at_async`, `delete_async`,
  `exists_async`, `metadata_async`, `is_file_async`,
  `is_dir_async`.
- Directory: `mkdir_async`, `mkdir_all_async`, `rmdir_async`,
  `rmdir_all_async`, `list_async`, `scan_async`,
  `scan_all_async`, `count_async`, `count_all_async`,
  `find_async`.
- Sync/batch: `sync_async`, `write_batch_async`,
  `delete_batch_async`, `sync_batch_async`.

### Calling async ops outside a runtime

Calling `*_async` from a thread with no active tokio runtime
returns `Error::AsyncRuntimeRequired` rather than panicking.

---

## `AsyncSubstrate` — runtime substrate observability

```rust
pub enum AsyncSubstrate {
    NativeIoUring,    // Linux + Method::Direct + ring active
    SpawnBlocking,    // Cross-platform fallback
    // #[non_exhaustive]
}

impl AsyncSubstrate {
    pub fn name(self) -> &'static str;       // "native io_uring" | "spawn_blocking"
    pub fn is_native(self) -> bool;
    pub fn is_blocking(self) -> bool;
}
```

Read at runtime via `Handle::async_substrate()`. New in 0.7.0.

### When you get `NativeIoUring`

All four conditions must hold:
1. Linux.
2. `Method::Direct` is the active method.
3. The per-handle io_uring ring has been constructed (built
   lazily on the first async Direct op).
4. `FSYS_DISABLE_NATIVE_ASYNC` is **not** set in the
   environment.

If any condition fails, the substrate is `SpawnBlocking`.

### `name()` is an accessor

`name()` returns the substrate's display name (e.g. `"native
io_uring"`). It is **not** an `as_str` conversion — there is
no underlying string storage. This is one-off naming
inconsistency vs. `Method::as_str` / `Mode::as_str`, kept
deliberately because the semantics are different (display
name vs. string-form-of-enum).

### `FSYS_DISABLE_NATIVE_ASYNC=1`

Set this environment variable to force `SpawnBlocking` even
on Linux + Direct. Useful for:
- A/B perf comparisons (run the same suite native vs.
  blocking).
- Diagnosing a suspected native-substrate issue without
  rebuilding.
- CI runners where io_uring is unavailable but you want
  consistent behaviour.

The override is read once on each `async_substrate()` call —
no re-export at process start needed.

### Why Linux-only

Per locked decision D-1, native fast-path async is **Linux
only**. Windows IOCP is technically capable but the
demand-to-engineering-cost ratio doesn't justify it for
0.7.0. macOS has no equivalent primitive. The asymmetry is
documented honestly rather than papered over with a fake
"native" path that's just `spawn_blocking` with extra steps.

### Bench delta

In our WSL2 + ext4 + Direct measurements, the native
substrate delivers a **1.46× throughput improvement** vs.
`spawn_blocking` at 4 KiB writes. Bare-metal Linux + NVMe is
expected to show 2×+ but we have not formalised that number.
See [`docs/BENCH.md`](BENCH.md) for the full methodology.

---

## `Error` and `Result`

```rust
pub type Result<T, E = Error> = std::result::Result<T, E>;
```

`Error` is the sole error type. Every variant carries a stable
`code` (`FS-NNNNN`) for log-grep without depending on the
display message format.

### Variants

| Code | Variant | Meaning |
|---|---|---|
| FS-00001 | `Io(io::Error)` | Underlying IO error. |
| FS-00002 | `InvalidPath { path, reason }` | Path escapes root, contains nul, etc. |
| FS-00003 | `UnsupportedMethod { method }` | E.g. `Method::Journal`. |
| FS-00004 | `AlignmentRequired { ... }` | Direct path with misaligned buffer/offset/length. |
| FS-00005 | `AtomicReplaceFailed { step, source }` | Step in atomic-rename sequence failed. |
| FS-00006 | `Configuration { reason }` | Builder validation error. |
| FS-00007 | `Pipeline { reason }` | Group-lane dispatcher error. |
| FS-00008 | `Capacity { kind }` | Buffer-pool / queue exhaustion. |
| FS-00009 | `BatchPartial(BatchError)` | Batch succeeded with per-op errors. |
| FS-00010 | `Tombstoned` | Handle was tombstoned by panic in completion driver. |
| FS-00011 | `AsyncRuntimeRequired` | Async method called with no tokio runtime. |
| FS-00012 | `GlobPatternInvalid { reason }` | `find()` pattern failed to compile. |
| FS-00013 | `IoUringSetupFailed { reason }` | `io_uring_setup(2)` returned ENOSYS / EPERM / etc. |
| FS-00014 | `MmapFailed { reason }` | `mmap()` rejected by FS. |
| FS-00015 | `MmapInvalidArgument { reason }` | E.g. file too small. |
| FS-00016 | `NvmePassthroughDenied { reason }` | NVMe admin op rejected. |
| FS-00017 | `NvmePassthroughUnavailable { reason }` | Driver path missing. |
| FS-00018 | `NvmeAdminFailed { reason }` | NVMe admin op runtime failure. |
| FS-00019 | `HandlePoisoned { reason }` | Mutex poisoned mid-op (rare). New in 0.7.0. |
| FS-00020 | `IoUringSubmitFailed { reason }` | Native async submission failed. New in 0.7.0. |
| FS-00021 | `CompletionDriverDead { reason }` | Native async completion driver task died. New in 0.7.0. |

`Error::code(&self) -> &'static str` returns the FS-NNNNN code
without allocating.

---

## Hardware / OS info accessors

### `fsys::hardware`

| Item | Returns |
|---|---|
| `info()` | Cached snapshot of all probe data. Returns `&'static HardwareInfo`. |
| `drive()` | Cached `&'static DriveInfo` (kind, sector size, capacity, queue depth, PLP). |
| `cpu()` | Cached `&'static CpuInfo` (logical cores, physical cores, frequency). |
| `memory()` | Live `MemoryInfo` snapshot (does *not* cache — memory availability changes constantly). |
| `io_primitives()` | Cached `&'static IoPrimitives` — fields `io_uring: bool`, `nvme_passthrough: bool`, etc. |

`HardwareInfo` (the type returned by `info()`) has public fields
`drive: DriveInfo`, `memory: MemoryInfo`, `cpu: CpuInfo`,
`io_primitives: IoPrimitives`.

### `fsys::os`

| Item | Returns |
|---|---|
| `info()` | Cached `OsInfo` (family, kind, arch, kernel, page size). |
| `family()` / `kind()` / `arch()` / `kernel_version()` / `page_size()` | Live accessors. |

---

## Path utilities

### `fsys::path`

| Item | Purpose |
|---|---|
| `default_data_dir()` / `default_cache_dir()` / `default_config_dir()` | OS-aware default paths (XDG on Linux, `~/Library/...` on macOS, `%APPDATA%` on Windows). |
| `normalize(path)` | Collapse `..` / `.` segments without touching the FS. |
| `sanitize_segment(s)` | Strip nul bytes, leading slashes, etc. from a single path component. |

`Mode::resolve()` consults the environment.

---

## Constants and primitives

`fsys::primitive` exposes static strings for every value
returned by `Handle::active_durability_primitive()`. Use these
constants in match expressions to avoid string typos:

```rust
use fsys::primitive::{FSYNC, FDATASYNC, IO_URING, F_FULLFSYNC};

match fs.active_durability_primitive() {
    FSYNC => /* ... */,
    FDATASYNC => /* ... */,
    IO_URING => /* ... */,
    F_FULLFSYNC => /* ... */,
    _ => /* fallback path */,
}
```

---

## Stability + breaking-change policy

- The API surface as documented above is **frozen** at the
  `0.9.0` release-candidate tag.
- Cargo SemVer allows minor-bump breaks within `0.x.y`, but
  the policy from `0.9.0` onward is **freeze and only break
  for genuine bugs**. The 0.9.0 RC carries the 0.7.0 rename
  audit forward verbatim and adds the journal substrate as
  net-new public surface.
- The `1.0.0` release will guarantee API stability per
  standard SemVer once real-world testing validates the
  current shape.

The H.5 audit
([`.dev/API-AUDIT-0.7.0.md`](../.dev/API-AUDIT-0.7.0.md))
is the original cleanup pass behind the freeze. Of ~120
public items reviewed at 0.7.0, the verdicts were ~117 Keep,
3 Rename, **0 Remove**. The 0.9.0 additions
(`JournalHandle`, `JournalReader`, `JournalOptions`,
`JournalRecord`, `JournalTailState`, `Lsn`, `Advice`,
`Handle::journal`, `Handle::journal_with`) extend that
surface without breaking it.

### What the freeze does NOT cover

- **Display formatting** of `Error::Display`, `Method::Display`,
  etc. is not part of the frozen surface — it can be tightened.
  Match on `Error::code()` for stable string contracts.
- **Default values** of `Builder` knobs (e.g.
  `buffer_pool_count = 64`) are tuning constants, not API
  contracts — they may shift under workload data.
- **Internal modules** (anything not re-exported at the crate
  root) are not stable. Add explicit imports of the public
  re-exports rather than reaching into submodules.

---

## Journal substrate

The journal is the high-throughput durability primitive that
storage engines, message queues, and ledgers use as their
write-ahead log. It is independent of [`Method`] — every
`Handle` can open a journal regardless of how the parent
handle was constructed.

### `JournalHandle`

```rust
use std::sync::Arc;
use fsys::builder;

let fs = builder().build()?;
let log = Arc::new(fs.journal("/var/log/app.wal")?);

// Many appends, no fsync per record.
let _lsn1 = log.append(b"event 1")?;
let _lsn2 = log.append(b"event 2")?;
let lsn3 = log.append(b"event 3")?;

// One group-commit fsync covers all three.
log.sync_through(lsn3)?;
# Ok::<(), fsys::Error>(())
```

| Method | Purpose |
|---|---|
| `Handle::journal(path)` | Open with default options (buffered / lock-free). |
| `Handle::journal_with(path, opts)` | Open with caller-supplied [`JournalOptions`]. |
| `JournalHandle::append(&[u8]) -> Lsn` | Append one framed record. Returns the LSN immediately past the record. |
| `JournalHandle::sync_through(Lsn)` | Group-commit fsync — make all bytes ≤ `lsn` durable. |
| `JournalHandle::synced_lsn() / next_lsn()` | Observability accessors. |
| `JournalHandle::preallocate(off, len)` | Reserve filesystem extents (Linux `fallocate(KEEP_SIZE)` / macOS `F_PREALLOCATE` / Windows `FileAllocationInfo`). |
| `JournalHandle::advise(off, len, Advice)` | Hint kernel about access pattern (Linux `posix_fadvise`). |
| `JournalHandle::is_direct_active() -> bool` | True when Direct-IO mode is engaged. |
| `JournalHandle::close(self)` | Final sync + close, with explicit error reporting. |

### `JournalOptions` (new in 0.9.0)

Opts the journal into Direct-IO mode and tunes the in-memory
log buffer size.

```rust
use fsys::{builder, JournalOptions};

let fs = builder().build()?;
let log = fs.journal_with(
    "/var/lib/mydb/wal",
    JournalOptions::new()
        .direct(true)             // O_DIRECT / FILE_FLAG_NO_BUFFERING
        .log_buffer_kib(256),     // larger buffer for sustained throughput
)?;
# Ok::<(), fsys::Error>(())
```

| Method | Default | Purpose |
|---|---|---|
| `JournalOptions::new()` | — | Library-default values. |
| `.direct(bool)` | `false` | Open with Direct-IO; route appends through a sector-aligned log buffer. |
| `.log_buffer_kib(u32)` | `64` | Log buffer size in KiB. Clamped to `[4, 65536]`. |

### `JournalReader`

Forward-streaming replay reader with checksum validation per
record and 5-state tail classification.

```rust
use fsys::{JournalReader, JournalTailState};

let mut reader = JournalReader::open("/var/log/app.wal")?;
for rec in reader.iter() {
    let r = rec?;
    println!("LSN {} payload {} bytes", r.lsn, r.payload.len());
}
match reader.tail_state() {
    JournalTailState::CleanEnd => { /* journal ended cleanly */ }
    JournalTailState::TruncatedHeader
    | JournalTailState::TruncatedPayload
    | JournalTailState::ChecksumMismatch => {
        // Recoverable — truncate at reader.position() and resume.
    }
    JournalTailState::BadMagic | JournalTailState::LengthOverflow => {
        // Format corruption — surface to a human operator.
    }
    _ => {}
}
# Ok::<(), fsys::Error>(())
```

`JournalReader::read_at_lsn(lsn)` does positioned reads;
`seek_to(lsn)` repositions the iterator. `advise_sequential()`
hints the kernel for replay workloads.

### Frame format

Every record is wrapped in a 12-byte frame: 4-byte big-endian
magic+version (`0x46535901`), 4-byte little-endian length,
payload, 4-byte little-endian CRC-32C (Castagnoli, RFC 3720).
Maximum payload: `(1 << 28) - 1` bytes (256 MiB). The
hardware-accelerated CRC-32C check provides single-bit-flip
detection — pinned empirically by an exhaustive
`property_single_bit_flip_detected` test.

---

## API changes in 0.9.0

The 0.9.0 release-candidate phase adds the journal substrate
and the Direct-IO opt-in. No breaking changes vs. the 0.8.0
alpha freeze; all additions are net-new public types and
methods.

### Additions (0.9.0)

- `pub struct JournalHandle` — open-once journal.
- `pub struct JournalOptions` (with `direct`, `log_buffer_kib`).
- `pub struct JournalReader` — replay reader.
- `pub struct JournalRecord`, `pub struct Lsn`.
- `pub enum JournalTailState` (`#[non_exhaustive]`).
- `pub enum Advice` — `Sequential`, `Random`, `WillNeed`,
  `DontNeed`, `Normal`.
- `Handle::journal(path)`, `Handle::journal_with(path, opts)`.
- `JournalHandle::{append, sync_through, synced_lsn, next_lsn,
   preallocate, advise, is_direct_active, close}`.
- Async siblings: `JournalHandle::{append_async, sync_through_async,
   native_iouring_active}` (gated behind `async` feature).
- New cargo feature `tracing` — opt-in `tracing::trace_span!`
  instrumentation on the journal append / sync_through paths.

### API changes in 0.7.0 (carried forward)

This was the **last breaking-change phase before alpha**. The
changes below landed at 0.7.0 and are frozen at the 0.9.0 RC.

### Renames

| 0.6.0 name | 0.7.0 name | Reason |
|---|---|---|
| `Handle::read_range(path, offset, len)` | `Handle::read_at(path, offset, len)` | Standard naming for offset+length read; `read_range` suggested an inclusive-exclusive range type. |
| `Handle::scan(path, recursive: bool)` | `Handle::scan(path)` + `Handle::scan_all(path)` | Bare-bool parameter at call site is a Rust API smell; split into two methods. |
| `Handle::count(path, recursive: bool)` | `Handle::count(path)` + `Handle::count_all(path)` | Same reason as `scan`. |
| `Builder::buffer_pool_size` | `Builder::buffer_pool_count` | "Size" implied byte total; this is a count of buffers. |
| `Builder::buffer_pool_block` | `Builder::buffer_pool_block_size` | "Block" alone was ambiguous; now explicitly the per-buffer size. |

The async siblings of the renamed methods (`read_at_async`,
`scan_async` / `scan_all_async`, `count_async` /
`count_all_async`) follow the same renames.

### Additions

- New `AsyncSubstrate` enum + `Handle::async_substrate()`.
- New `Error` variants: `HandlePoisoned`, `IoUringSubmitFailed`,
  `CompletionDriverDead`.
- New `FSYS_DISABLE_NATIVE_ASYNC` environment override.
- Native io_uring async substrate (Linux + `Method::Direct`).

### Strengthened docstrings (no signature change)

- `Method::Sync` — leads with the "not 'synchronous IO'"
  disambiguation.
- `Method::Journal` — reframed from "reserved for 0.7.0" to
  "reserved indefinitely; no committed version."
- `Handle::write_copy` — leads with "this is NOT a
  file-to-file copy."
- `Handle::find` — explicit recursion-by-pattern section
  contrasting with the `scan` / `scan_all` flat-vs-recursive
  split.

### Removals

None. The audit pass produced zero Removes — the surface is
in healthy shape for alpha freeze.

---

## See also

- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — internal
  layering and concurrency model.
- [`docs/CRASH-SAFETY.md`](CRASH-SAFETY.md) — durability
  guarantees per method per platform.
- [`docs/METHODS.md`](METHODS.md) — choosing the right
  `Method` for your workload.
- [`docs/PERFORMANCE.md`](PERFORMANCE.md) — tuning the
  group-lane dispatcher and buffer pool.
- [`docs/PLATFORM-NOTES.md`](PLATFORM-NOTES.md) — per-OS
  quirks.
- [`docs/BENCH.md`](BENCH.md) — methodology + how to run
  benches.
- [`CHANGELOG.md`](../CHANGELOG.md) — version-by-version
  change log.

[`std::fs::copy`]: https://doc.rust-lang.org/std/fs/fn.copy.html
