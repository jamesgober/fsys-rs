# Architecture

`fsys` is layered to keep concerns minimal at each level. Each layer
depends only on layers below it; cross-layer shortcuts are
prohibited.

```
┌─────────────────────────────────────────────────────────────┐
│  Public API: builder() / new() / with() / Handle / quick::* │
└────────┬────────────────────────────────┬───────────────────┘
         │                                │
         ▼                                ▼
┌─────────────────┐              ┌───────────────────┐
│  CRUD modules   │              │  Async layer      │  (feature)
│  (file, dir,    │              │  spawn_blocking   │
│   batch, async) │◀─────────────│  + oneshot batch  │
└────────┬────────┘              └───────────────────┘
         │
         ▼
┌─────────────────────────────────────┐
│  Method backends (sync, data,       │
│   direct, mmap, auto)               │
└────────┬────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────┐
│  Pipeline: solo + group dispatcher  │
│   (per-handle thread, BatchResponse │
│    enum: Sync | Async)              │
└────────┬────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────┐
│  Platform: linux / macos / windows  │
│   (atomic-replace primitives,       │
│    Direct IO, NVMe passthrough,     │
│    io_uring on Linux)               │
└────────┬────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────┐
│  Hardware probe + OS info + paths   │
└─────────────────────────────────────┘
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
  `Mmap`, `Direct`, `Journal` (reserved 0.7.0), `Auto`).
- **`crate::Builder`** — three-tier API (one-shot `quick::*`,
  default `new()`/`with(method)`, builder for advanced config).
- **`crate::pipeline`** — per-handle dispatcher serving the
  group-lane batch API. The dispatcher is a single thread spawned
  lazily on first batch op.
- **`crate::primitive`** — public constants for the strings
  returned by `Handle::active_durability_primitive()`. Match
  against these to avoid string typos.
- **`crate::async_io`** — async wrappers (gated behind the `async`
  Cargo feature). Single-op CRUD via `tokio::task::spawn_blocking`;
  batch via `tokio::sync::oneshot` through the same dispatcher.

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
   in `BatchResponse::Async`, and pushes a `BatchJob` onto the
   dispatcher's bounded queue.
3. The dispatcher thread (already spawned, single per-handle)
   pulls the job, executes ops in submission order with per-op
   `catch_unwind`, builds the result, and calls
   `BatchResponse::send(result)` — which routes to the oneshot
   sender for this job.
4. The async caller awakens from `oneshot::recv().await`.

## Concurrency model

- `Handle` is `Send + Sync`. Multiple threads share a single
  handle; the dispatcher and other internal slots use `Mutex<...>`
  with brief lock scopes.
- The dispatcher is single-threaded by design — it serialises
  durability operations within its lane. Solo-lane writes (single
  ops via `Handle::write` etc.) bypass the dispatcher and run on
  the calling thread.
- Async submission shares the dispatcher with sync submission via
  the `BatchResponse` enum — no second dispatcher, no second queue.
- Dispatcher thread is joined cleanly on `Handle::drop`.

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
