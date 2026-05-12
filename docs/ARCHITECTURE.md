<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  ARCHITECTURE
</h1>

`fsys` is layered to keep concerns minimal at each level. Each layer
depends only on layers below it; cross-layer shortcuts are
prohibited. Diagram reflects the architecture as of **0.9.7**.

```
┌─────────────────────────────────────────────────────────────────┐
│  Public API: builder() / new() / with() / Handle / quick::*     │
│              + Handle::journal() / journal_with()  (0.9.0)      │
└────────┬────────────────────────────┬───────────────────────────┘
         │                            │
         ▼                            ▼
┌─────────────────┐  ┌──────────────────────────────────┐
│  CRUD modules   │  │  Async layer (feature `async`)   │
│  (file, dir,    │  │                                  │
│   batch)        │◀─│  Substrate selection:            │
│                 │  │   • NativeIoUring (Linux+Direct) │
│ + Journal       │  │   • SpawnBlocking (everywhere)   │
│   substrate     │  └────────┬─────────────────────────┘
│   (0.9.0)       │           │
└────────┬────────┘           ▼
         │           ┌─────────────────────────────┐
         │           │  Completion driver task     │
         │           │  (eventfd + tokio AsyncFd,  │
         │           │   one per Handle, lazy)     │
         │           └─────────┬───────────────────┘
         │                     │
         │           ┌─────────┴───────────────────┐
         │           │  FsysObserver hook (0.9.2)  │
         │           │  on_journal_{append,sync}   │
         │           │  on_handle_{write,read}     │
         │           └─────────────────────────────┘
         ▼
┌─────────────────────────────────────────┐
│  Method backends (sync, data, direct,   │
│   mmap, auto)                           │
└────────┬────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────┐
│  Pipeline: solo + group dispatcher          │
│   N independent dispatcher threads          │
│   (Builder::dispatcher_shards, 0.9.3)       │
│   BatchResponse: Sync | Async               │
└────────┬────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────┐
│  Platform: linux / macos / windows          │
│   atomic-replace, Direct IO, NVMe pass-     │
│   through, io_uring elite flags + WRITE_    │
│   FIXED + REGISTER_FILES (0.9.4–0.9.7),     │
│   APFS clonefile / ReFS reflinks (0.9.6),   │
│   punch_hole / write_zeros (0.9.5)          │
└────────┬────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────┐
│  Hardware probe + OS info + paths           │
│   PLP detection (0.7.0+)                    │
│   NAWUN / atomic_write_unit (0.9.4)         │
│   Runtime CPU-feature detection (0.9.2)     │
└─────────────────────────────────────────────┘
```

## Modules

- **`crate::os`** — operating system identification: family, kind,
  arch, kernel version, page size. Probed once via `std::sync::OnceLock`.
- **`crate::hardware`** — drive type, PLP, sector sizes, queue
  depth, capacity, IO primitive availability. Live functions for
  drive/memory/cpu; cached `info()` snapshot for everything-at-once.
- **`crate::path`** — OS-aware default paths, normalization,
  segment sanitisation, dev/prod mode.
- **`crate::Handle`** — the primary entry point. Owns the resolved
  method, root, mode, sector size, pipeline, buffer pool slot,
  io_uring slot (Linux), NVMe-passthrough slot (Linux + Windows).
- **`crate::Method`** — durability strategy enum (`Sync`, `Data`,
  `Mmap`, `Direct`, `Journal` (reserved variant — see note in
  [`METHODS.md`](METHODS.md)), `Auto`).
- **`crate::journal`** — open-once append-only log substrate
  (shipped in 0.9.0). Independent of `Method`; opened via
  `Handle::journal` / `Handle::journal_with` regardless of the
  parent handle's method. Three throughput tiers
  (cross-platform sync, lock-free POSIX/Windows append, native
  io_uring async on Linux) plus an opt-in Direct-IO mode that
  routes appends through a **dual-buffered** sector-aligned
  log buffer (0.9.5 — dual-buffer decouples appends from
  in-flight flushes, lifting Direct mode from a single-core
  ceiling to multi-core scalable). On Linux + Direct mode, the
  log-buffer flush submits via `IORING_OP_WRITE_FIXED` against
  pre-registered `AlignedBuf` slots (0.9.6 — saves per-SQE
  kernel-side buffer pinning). Production-grade frame format
  (magic + length + CRC-32C) and five-state tail-truncation
  taxonomy enable correct recovery-after-crash semantics.
  Group-commit fsync uses a leader/follower coordinator with
  the wake path on lock-free atomics (0.9.7 H-16 fix —
  ~5× lock-hold reduction under 100+ follower stampedes).
- **`crate::Builder`** — three-tier API (one-shot `quick::*`,
  default `new()`/`with(method)`, builder for advanced config).
  0.9.2 adds `observer(...)` and `tune_for(Workload)`; 0.9.3
  adds `dispatcher_shards(N)`; 0.9.7 adds `sqpoll(idle_ms)`.
- **`crate::pipeline`** — per-handle dispatcher serving the
  group-lane batch API. **N independent dispatcher threads** per
  handle (`Builder::dispatcher_shards`, 0.9.3); batches
  hash-routed by first op's path so within-batch order is
  preserved while concurrent submitters writing to different
  files scale near-linearly with shard count. Default `N=1`
  preserves pre-0.9.3 single-thread serialised behavior exactly.
- **`crate::observer`** — `FsysObserver` trait (0.9.2). The
  observability hook surface: typed per-op events for journal
  append / sync and handle write / read. Fires on the
  originating thread (no scheduling overhead). Per-op cost when
  no observer is registered: a single `Option::is_some` branch.
- **`crate::primitive`** — public constants for the strings
  returned by `Handle::active_durability_primitive()`. Match
  against these to avoid string typos.
- **`crate::async_io`** — async wrappers (gated behind the `async`
  Cargo feature). Single-op CRUD has two substrates as of 0.7.0:
  the **native io_uring substrate** (Linux + `Method::Direct`
  + ring active + no `FSYS_DISABLE_NATIVE_ASYNC`) submits
  directly to the per-handle ring and `.await`s a `oneshot`
  driven by a per-handle completion driver task, while the
  **`spawn_blocking` fallback** (every other configuration)
  hops a thread-pool. Read which one a handle uses via
  `Handle::async_substrate()`. Async batch routes through the
  group-lane dispatcher via `tokio::sync::oneshot` regardless
  of substrate.
- **`crate::substrate`** — `AsyncSubstrate` enum
  (`NativeIoUring` / `SpawnBlocking`). New in 0.7.0.

## Data flow — sync write

1. User calls `fs.write("/path", &data)`.
2. `Handle::write` resolves the path against the configured root
   (rejecting escapes), generates a temp path, opens it via
   `platform::open_write_new`.
3. The Direct path (when active) routes through `direct_write`,
   which on Linux first tries the per-handle io_uring ring (with
   NVMe passthrough flush if capable), falling back to the
   platform's `write_all_direct` (`pwrite` + `fdatasync`).
4. `platform::atomic_rename` performs the temp→target swap.
5. `platform::sync_parent_dir` finalises directory durability
   on Linux/macOS.

## Data flow — async batch

1. User calls `fs.write_batch_async(vec![...]).await`.
2. `write_batch_async` resolves all paths, builds a `Vec<BatchOp>`,
   constructs a `tokio::sync::oneshot::channel`, wraps the sender
   in `BatchResponse::Async`, and pushes a `BatchJob` onto one of
   the dispatcher shards' bounded queues. With `dispatcher_shards`
   > 1, the shard is chosen by hashing the batch's first op path
   so within-batch order is preserved per-path.
3. The dispatcher thread for that shard pulls the job, executes
   ops in submission order with per-op `catch_unwind`, builds the
   result, and calls `BatchResponse::send(result)` — which routes
   to the oneshot sender for this job.
4. The async caller awakens from `oneshot::recv().await`.

## Data flow — `Batch::commit_grouped` (0.9.3)

`commit_grouped()` is the atomic-batch fsync variant of `commit()`.
The dispatcher recognises the grouped semantics and amortises the
parent-directory `fsync` across the entire batch:

1. The batch's ops execute through the dispatcher as normal.
2. Instead of issuing one `fsync` per modified parent directory
   per op, the dispatcher collects the set of distinct parent
   directories touched by the batch.
3. After all ops complete, it issues **one** `fsync` per unique
   parent directory.

For bulk-load / SST-flush / checkpoint workloads where the
batch is the durability unit, this collapses N parent-dir
syncs into M (where M is the number of distinct parent
directories, typically 1).

## Concurrency model

- `Handle` is `Send + Sync`. Multiple threads share a single
  handle; the dispatcher shards and other internal slots use
  `Mutex<...>` with brief lock scopes.
- The dispatcher pool is `N` threads (default `N=1` preserves
  pre-0.9.3 behavior; raise via `Builder::dispatcher_shards`).
  Each shard owns a bounded queue and serialises durability ops
  within its lane. Cross-shard ordering across batches is not
  guaranteed — but it was never guaranteed at the pipeline level
  anyway.
- Solo-lane writes (single ops via `Handle::write` etc.) bypass
  the dispatcher entirely and run on the calling thread.
- Async submission shares the dispatcher with sync submission via
  the `BatchResponse` enum — no second dispatcher pool, no second
  queue.
- Journal append is **lock-free** across threads (atomic LSN
  reservation via `AtomicU64::fetch_add` with `Release`
  ordering — 0.9.7 M-2 tightened from `AcqRel`). Concurrent
  `pwrite` to distinct offsets is POSIX-atomic per call.
- Journal `sync_through` uses leader/follower group-commit. Many
  callers waiting on the same target LSN coalesce into one fsync
  syscall; followers wake via atomic-decrement + atomic-check on
  `synced_lsn`, skipping the state mutex on the common-case fast
  path (0.9.7 H-16).
- Dispatcher threads (all `N`) are joined cleanly on
  `Handle::drop`.

## Shut-down

Drop semantics:
1. `Handle::drop` runs.
2. The pipeline takes its inner state, signals shutdown to the
   dispatcher's job channel.
3. The dispatcher drains in-flight batches, sends final responses
   on each `BatchResponse`, then exits.
4. The pipeline's `Drop` joins the dispatcher thread.

Idle handles cost zero threads and zero ring memory — every
resource (dispatcher, buffer pool, io_uring ring, NVMe passthrough
access) is lazily allocated on the first op that needs it.
