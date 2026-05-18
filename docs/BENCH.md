<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  BENCHMARKS
</h1>

`fsys` benchmarks live in [`benches/`](../benches/) and use [Criterion](https://docs.rs/criterion). For tuning knobs (group-lane window, batch size, queue depth, buffer pool) see [`PERFORMANCE.md`](PERFORMANCE.md).

## Running

```sh
cargo bench --bench method_payload_matrix
cargo bench --bench mmap_workloads
cargo bench --bench direct_iouring
cargo bench --bench async_native_vs_blocking   # 0.7.0
cargo bench --bench tail_validation            # 0.7.0
cargo bench --bench matrix_with_peers --features async  # 0.8.0 F (peer comparison)
cargo bench --bench journal_vs_atomic_replace  # 0.9.0 R-1 (journal substrate)
cargo bench --bench batch_throughput           # batch dispatcher
cargo bench --bench concurrent_batches         # multi-shard dispatcher (0.9.3)
cargo bench --bench solo_vs_batch              # solo vs batched cost
cargo bench --bench capability_access          # 1.1.0 — capability cache cold/warm
cargo bench --bench backend_accessors          # 1.1.0 — backend_kind/health/info
```

`cargo bench` (no `--bench`) runs the full Criterion suite. Build with `--features async` for the async-substrate benches.

## Methodology

- **Criterion** &mdash; default 3 s warmup, 5 s measurement, 100 samples per group. Override per-bench via env: `CRITERION_WARMUP_TIME=1s CRITERION_MEASUREMENT_TIME=10s`.
- **Workload sizes.** 4 KiB, 64 KiB, 1 MiB, 16 MiB. The 4 KiB point matches a typical FS block and is the most sensitive to fixed overhead.
- **Per-method matrix.** Every workload runs against `Sync`, `Data`, `Direct` to keep the baselines comparable. `Mmap` is benched separately (read-heavy random-access workloads).
- **Repeatability.** Benches assume an idle machine, ondemand or performance CPU governor pinned, no concurrent IO. CI-runner numbers are not comparable to bare-metal numbers; see "Hardware classes" below.

## 0.7.0 regression-budget infrastructure

[`benches/baselines.json`](../benches/baselines.json) holds per-machine-class baselines plus regression thresholds. Strictness is **hybrid**:

- **Critical metrics** (atomic-replace latency, async-overhead): &le; 5 % regression.
- **Standard metrics** (per-method throughput): &le; 10 %.
- **Loose metrics** (mmap reads, find/scan): &le; 25 %.

Tail latency uses a **relative** target ([`benches/tail_validation.rs`](../benches/tail_validation.rs)): p99.9 must stay within 10&times; p50. Relative targets are portable across hardware classes; absolute targets are not.

### Hardware classes

Baselines are per-class, not absolute. Current classes:

- `wsl-ext4-nvme` &mdash; reference dev environment.
- `linux-bare-nvme` &mdash; Linux on bare metal with NVMe (target for `0.9.x` post-RC certification).
- `windows-ntfs-nvme` &mdash; Windows + NTFS on NVMe.
- `macos-apfs-nvme` &mdash; macOS + APFS on Apple-silicon NVMe.

Numbers from one class are not directly comparable to another; the regression check happens within-class only.

## 0.7.0 native-vs-`spawn_blocking` measurement

[`benches/async_native_vs_blocking.rs`](../benches/async_native_vs_blocking.rs) is the A/B harness for the new native io_uring async substrate. It runs the same async-write workload twice &mdash; once with `FSYS_DISABLE_NATIVE_ASYNC=1` (forces the `spawn_blocking` fallback), once without (allows the native path to engage on Linux + `Method::Direct`).

The Linux substrate is required to be at least 1.1&times; faster than `spawn_blocking` to be considered functional (anything below means something went wrong with the ring construction or completion driver). Anything above 1.1&times; is a real win.

**Measured to date:** 1.46&times; on `wsl-ext4-nvme` at 4 KiB writes (`0.7.0` checkpoint G). Bare-metal Linux + NVMe is expected to be 2&times;+ but has not yet been formalised &mdash; that measurement is on the `0.8.0` checklist.

## Tail-validation harness

[`benches/tail_validation.rs`](../benches/tail_validation.rs) is a sample-and-percentile harness independent of Criterion. It captures 100 K samples per workload, computes p50/p95/p99/p99.9, and asserts the relative-tail rule (p99.9 &le; 10&times; p50). Useful for catching long-tail GC-style stalls that average-throughput benches hide.

## 0.8.0 F+I-checkpoint certified results — `windows-ntfs-nvme`

> **Run date:** 2026-05-05 (UTC). Captured after the I-checkpoint
> perf-tuning pass (Direct-IO truncate-via-reopen → in-place
> `set_len`; mmap-write fsync; `write_all_direct` partial-write
> looping). Compare against the pre-I numbers in
> [`Performance evolution within 0.8.0`](#performance-evolution-within-080)
> below.
> **Host:** Windows 11 Pro, x86_64.
> **Storage:** Local NVMe SSD; `std::env::temp_dir()` resolves to NTFS.
> **fsys version:** 0.7.0 (pre-0.8.0 freeze) plus checkpoint-B + checkpoint-I patches.
> **Source:** [`benches/matrix_with_peers.rs`](../benches/matrix_with_peers.rs).
> **Iterations:** 100 timed iterations after 10 warmup. Cell format: median µs / p99 µs (or ms when ≥ 1000 µs).
> **Run-to-run noise:** ±5 % on Windows; numbers below are stable across two consecutive runs.

### Single write (atomic-replace) — `fsys` methods vs `std::fs::write`

| Payload | `fsys::Sync` | `fsys::Data` | `fsys::Direct` | `fsys::Auto` | `std::fs::write` |
|---------|-------------:|-------------:|---------------:|-------------:|-----------------:|
| 4 KiB | 1.07 ms / 4.78 ms | 1.12 ms / 7.47 ms | 1.23 ms / 5.97 ms | 1.08 ms / 4.69 ms | 218.7 / 7181.9 |
| 64 KiB | 1.29 ms / 4.33 ms | 1.32 ms / 8.83 ms | 1.35 ms / 4.90 ms | 1.23 ms / 5.50 ms | 4.48 ms / 5.47 ms |
| 1 MiB | 1.83 ms / 7.55 ms | 1.84 ms / 5.29 ms | 1.68 ms / 5.57 ms | 1.80 ms / 5.00 ms | 2.84 ms / 16.45 ms |

**Honest interpretation.** `std::fs::write` wins at the **median** for 4 KiB writes (≈ 220 µs vs `fsys::Auto`'s 1.08 ms — `std::fs` is ~5× faster on this single-cell metric). `fsys::Auto` wins at **every other cell**: 4× faster at 64 KiB, 1.6× faster at 1 MiB. The reason `std::fs::write` looks faster on small writes is that it does **not provide durability guarantees** — there's no `fsync` after the buffered write completes, so a power-loss event mid-call can leave the file partial-or-missing on disk. The `fsys` numbers include the full atomic-replace cycle: open temp + write + `fsync`/`fdatasync` + atomic rename + parent-dir sync. That's the durability tax.

**At p99**, the gap inverts decisively. `std::fs::write`'s 4 KiB p99 is 7.18 ms — 33× its own median; `fsys::Auto`'s 4 KiB p99 is 4.69 ms (4.4× its median). At 1 MiB, `std::fs::write` p99 is 16.45 ms; `fsys::Auto` is 5.00 ms — **3.3× faster on tail latency**. This matches the design intent: `fsys` pays a deterministic durability cost on every op so the tail is bounded, while `std::fs::write` defers flushing to OS scheduling and pays unpredictably.

If you want `fsys`-equivalent durability from `std::fs`, you'd write to a temp file, call `file.sync_all()`, then `std::fs::rename` — and pay roughly the same cost as `fsys::Sync`. The fair comparison is not "Method::Sync vs std::fs::write" but "Method::Sync vs std::fs + manual atomic-replace dance"; the latter is what most application code gets wrong.

### Full-file read — `fsys::Auto` vs `std::fs::read` vs `tokio::fs::read`

| Payload | `fsys::Auto` | `std::fs::read` | `tokio::fs::read` (via `spawn_blocking`) |
|---------|-------------:|----------------:|---------------------------------------:|
| 4 KiB | 25.0 / 89.4 | 23.7 / 77.1 | 35.8 / 152.8 |
| 64 KiB | 25.0 / 58.9 | 24.1 / 64.0 | 105.9 / 337.5 |
| 1 MiB | 182.5 / 482.3 | 189.0 / 327.4 | 250.7 / 585.8 |

**Honest interpretation.** Reads are essentially tied with `std::fs::read` (within 5 % on the median — `fsys::Auto`'s read path is `std::fs::read` plus negligible handle bookkeeping; the 1 MiB case has fsys actually ~3 % faster). `tokio::fs::read` (simulated via `spawn_blocking` against `std::fs::read` — what tokio's own `fs` module does internally) is 1.5×–4.4× slower because of the thread-pool hop. The 0.7.0 native io_uring async substrate (Linux + Direct + async feature) bypasses that hop on Linux — see the 0.7.0 G-checkpoint result above.

### `write_copy` (atomic-replace + metadata preservation)

| Payload | `fsys::Sync` `write_copy` | `fsys::Auto` `write_copy` |
|---------|--------------------------:|--------------------------:|
| 4 KiB | 1.32 ms / 4.40 ms | 1.33 ms / 4.55 ms |
| 64 KiB | 1.54 ms / 14.21 ms | 1.80 ms / 5.21 ms |
| 1 MiB | 2.33 ms / 5.87 ms | 2.26 ms / 9.28 ms |

**Honest interpretation.** `write_copy` is ~25 % slower than `write` at the median (1.32 ms vs 1.07 ms for `Sync` 4 KiB). That's the expected cost of the metadata-preservation work — `read` of existing meta + `chmod`/`chown`/timestamp restore on the temp file before rename. Both APIs have the same atomic-replace contract (target file is either entirely-old or entirely-new, never torn), so the ~250 µs gap is the cost of preservation. Use `write` when no metadata preservation is needed; use `write_copy` when replacing config files whose mode/ACL/owner is set externally.

### Batch-of-8 writes vs. 8 solo writes (per-op cost)

Numbers below reflect the **I round-2 dispatcher fast-flush fix**. Pre-fix numbers (with the 1 ms accumulation window adding fixed latency on every batch) are recorded in [`Performance evolution within 0.8.0`](#performance-evolution-within-080) below.

| Payload | `fsys` batch-of-8 (per-op µs) | `fsys` 8× solo writes (per-op µs) | speedup |
|---------|------------------------------:|----------------------------------:|--------:|
| 4 KiB | 931.7 / 6366.6 | 927.3 / 1276.3 | **1.00×** |
| 64 KiB | 1.12 ms / 1.87 ms | 1.06 ms / 1.46 ms | 0.95× |
| 1 MiB | 1.83 ms / 2.25 ms | 1.73 ms / 3.33 ms | 0.95× |

**Honest interpretation.** Batch is now **essentially tied with solo×8** at the median (within 5 % across all payload sizes). The 1.00× speedup at 4 KiB is the right answer for "1 op per file with no syscall-batching benefit on this platform" — Windows `FlushFileBuffers` doesn't have a kernel-batch primitive analogous to Linux io_uring SQEs, so we don't expect a *speedup* here. We expect **at-cost equivalence**, which the dispatcher fast-flush now delivers.

The pre-fix numbers (0.42–0.65× of solo) were the result of the dispatcher's 1 ms accumulation window adding ~500 µs of fixed latency to every batch — a window that's only useful when multiple submitters race jobs into the queue. For a single-submitter "submit one batch and wait" pattern (which the bench measures), the window was pure overhead. The I-round-2 fix detects this case via a non-blocking `try_recv` pass at the top of the dispatcher loop and skips the window when no concurrent jobs are queued.

Linux + io_uring remains the regime where batch *beats* solo (the kernel coalesces SQEs); Windows is at-cost. On both platforms the dispatcher no longer pays a fixed-latency window tax for the trivial case.

---

## Performance evolution within 0.8.0

The B-checkpoint code audit + I-checkpoint perf tuning produced
measurable improvements on the F-bench. Headline before/after on
`windows-ntfs-nvme` at the 4 KiB payload (median):

| Method | Pre-I (B-checkpoint baseline) | Post-I (current) | Speedup |
|--------|------------------------------:|-----------------:|--------:|
| `fsys::Sync` | 3.79 ms | 1.07 ms | **3.5×** |
| `fsys::Data` | 2.92 ms | 1.12 ms | **2.6×** |
| `fsys::Direct` | 3.14 ms | 1.23 ms | **2.5×** |
| `fsys::Auto` | 2.55 ms | 1.08 ms | **2.4×** |

The **single load-bearing fix** that drove the Direct path
improvement: replacing the post-write "drop the `O_DIRECT` /
`FILE_FLAG_NO_BUFFERING` handle, reopen buffered, call `set_len`"
pattern with an in-place `set_len` call on the already-open file
handle. `set_len` works on Direct-IO file handles on every
platform; the prior pattern wasted two syscalls (close + open)
per Direct write. The other methods improved via the same
truncation path being taken on the buffered side after sector
padding (the cross-method speedup pattern is consistent with that
single-fix story).

The I checkpoint also fixed two **correctness** issues that don't
show on the median bench:

- **`mmap` write missing `fsync`**: `msync(MS_SYNC)` flushes data
  pages on Linux/macOS but does NOT include a metadata sync.
  Without `fsync` after `msync`, the renamed file could have
  data on disk but stale size metadata after a power-loss event.
  Now adds `temp_file.sync_all()` between `msync` and `rename`.
- **`write_all_direct` partial-write looping**: `pwrite(2)` may
  return less than requested on EINTR or short-write conditions
  on certain filesystems. The 0.7.0 code did a single pwrite and
  trusted the return value; large Direct writes could silently
  truncate. Now loops on partial writes with EINTR handling.

These don't appear in the F bench (the bench environment doesn't
exercise the EINTR or stale-size paths) but are recorded here
because they affect the correctness contract the bench numbers
*assume*.

### I round-2 — dispatcher fast-flush + alloc reductions

The I round-2 pass closed the **batch-slower-than-solo** gap that the F bench surfaced as a real finding.

| Workload (4 KiB payload) | Pre-I-round-2 | Post-I-round-2 | Speedup |
|--------------------------|--------------:|---------------:|--------:|
| `write_batch` (8 ops, per-op µs) | 1.93 ms | 931.7 µs | **2.07×** |
| `write_batch` (8 ops, 64 KiB, per-op) | 1.92 ms | 1.12 ms | 1.71× |
| `write_batch` (8 ops, 1 MiB, per-op) | 3.82 ms | 1.83 ms | 2.09× |

The fix is a **non-blocking `try_recv` drain at the top of the
group-lane dispatcher loop**. Before:

1. Dispatcher waits blocking for the first job.
2. On arrival, starts a `batch_window_ms` deadline (default 1 ms).
3. Loops on `select!` waiting for more jobs OR the deadline.
4. On deadline expiry or `batch_size_max`, flushes.

For the bench's "submit one batch and wait" pattern — and any
similar "single submitter, periodic batches" workload — step 3
adds ~500 µs of pure latency (average of the uniform window
distribution) because no other submitter is racing.

After:

1. Dispatcher waits blocking for the first job.
2. On arrival, **immediately drains any already-queued jobs via
   `try_recv`** (non-blocking).
3. If the drain found additional jobs (multiple submitters
   actively racing), enter the time-window to catch trickling
   late arrivals.
4. If the drain found nothing (single-submitter case), flush
   immediately — no window wait.

Concurrent-submitter workloads still get the original batching
behaviour (their jobs are scooped in step 2 OR caught in step 3's
window). Single-submitter workloads avoid the window tax. **Net:
no regression for any workload type, ≈ 2× speedup for the bench
shape.**

`gen_temp_path` was also tightened — replaced
`format!(".fsys-tmp-{}.{}", n, stem.to_string_lossy().into_owned())`
+ `parent.join(name)` (3 string allocations + 1 PathBuf alloc per
write) with a direct `OsString` build (1 OsString + 1 PathBuf).
Stays in `OsStr`-land for non-UTF-8 filenames. The save is
~50–100 ns per write — too small to surface on the bench median
but cumulative across all writes.

---

## Journal substrate (0.9.0 R-1) — vs atomic-replace

The atomic-replace primitive caps around 200–500 K writes/sec on bare-metal Linux + NVMe (5–7 syscalls per write × 1–5 µs each). For database WAL workloads requiring millions of durable writes/sec, fsys 0.9.0 ships a **journal substrate** — open-once log file + atomic LSN reservation + group-commit fsync, with an opt-in Direct-IO mode that routes appends through a sector-aligned in-memory log buffer. See [`docs/API.md`](API.md#journal-substrate) for the API and [`src/journal/mod.rs`](../src/journal/mod.rs) for the implementation.

**Benchmark:** [`benches/journal_vs_atomic_replace.rs`](../benches/journal_vs_atomic_replace.rs) — measures the journal's single-threaded throughput against `Handle::write`'s atomic-replace baseline at the same payload size, across three sync cadences.

### Run date 2026-05-05 — `windows-ntfs-nvme`

#### Payload: 64 B (row-write workload)

| Method | Sync cadence | Per-op | Throughput | vs atomic-replace |
|--------|--------------|-------:|----------:|------------------:|
| atomic-replace | every write | 1.58 ms | 634 ops/s | 1.00× |
| journal (tier-1) | every append | 392.99 µs | 2.5 K ops/s | 4.01× |
| journal (tier-1) | every 100 appends | 6.18 µs | 161.9 K ops/s | **255×** |
| journal (tier-1) | once at end | 2.16 µs | 462.9 K ops/s | **730×** |

#### Payload: 4 KiB (page-write workload)

| Method | Sync cadence | Per-op | Throughput | vs atomic-replace |
|--------|--------------|-------:|----------:|------------------:|
| atomic-replace | every write | 1.12 ms | 891 ops/s | 1.00× |
| journal (tier-1) | every append | 397.27 µs | 2.5 K ops/s | 2.82× |
| journal (tier-1) | every 100 appends | 10.34 µs | 96.8 K ops/s | **109×** |
| journal (tier-1) | once at end | 5.28 µs | 189.3 K ops/s | **212×** |

### Honest interpretation

**The headline cadence** is "once at end" — the canonical WAL pattern of "append many records, fsync at a transaction boundary." At 64 B records (typical for a row-level transaction log) the journal is **730× faster** than atomic-replace on the same hardware. At 4 KiB (typical for a page-level log) it's **212×** faster. For database storage engines targeting millions of writes/sec this is the load-bearing primitive — the atomic-replace path was the wrong shape for the workload, period.

**Why the absolute numbers aren't yet the user-facing 5–10 M/sec target.** The tier-1 implementation is cross-platform sync — `Mutex<File>` + atomic LSN reservation + standard `pwrite`. It has three remaining bottlenecks:

1. **`Mutex<File>` serialises the underlying pwrite calls.** POSIX `pwrite` is concurrent-safe per call for sub-page records; tier-2 will go lock-free on the append path by going through the raw fd directly. Estimated 2–5× speedup for multi-threaded append workloads.
2. **One syscall per append.** Linux io_uring with SQE batching submits N appends as one syscall (+ one fsync SQE for the group commit). Tier-3 will use registered buffers + registered files + polling completion driver to eliminate syscalls in the steady-state hot path entirely. Estimated 10–50× speedup on Linux.
3. **The Windows numbers above are an underestimate of the Linux ceiling.** Windows `WriteFile` is not as cheap as Linux `pwrite`; bare-metal Linux + NVMe at the tier-1 single-threaded level should already hit 1–3 M ops/sec. Fire the [`bench.yml`](../.github/workflows/bench.yml) workflow to capture the canonical Linux numbers (the workflow registers this bench too).

**Tiering roadmap:**

| Tier | Implementation | Target throughput |
|------|----------------|------------------:|
| **Tier 1 (shipped 0.9.0)** | Cross-platform synchronous core: atomic LSN cursor + `pwrite` (POSIX) / `WriteFile`+`OVERLAPPED` (Windows) + group-commit fsync. | 100 K – 500 K ops/s (Windows), 1 M – 3 M (bare Linux). |
| **Tier 2 (shipped 0.9.0)** | Lock-free append. Concurrent `pwrite` directly against `&File` (no mutex on the hot path). 0.9.1 added vectored `append_batch` for ~1.6× per-record reduction. 0.9.5 added the dual-buffer Direct-mode log buffer for multi-core scalable Direct appends. | + 2–5× on multi-threaded workloads vs Tier 1. |
| **Tier 3 (shipped 0.9.0)** | Native io_uring asynchronous substrate on Linux + `async` feature. `IORING_OP_WRITE` / `IORING_OP_FSYNC(DATASYNC)` SQEs through the per-handle completion driver. No `spawn_blocking` thread-pool hop. | + 1.5–3× on Linux async workloads vs Tier 2. |
| **Direct-IO mode (shipped 0.9.0)** | Opt-in via `JournalOptions::direct(true)`. Sector-aligned in-memory log buffer; `O_DIRECT` / `F_NOCACHE` / `FILE_FLAG_NO_BUFFERING`; zero-copy DMA into the device. 0.9.6 added `IORING_OP_WRITE_FIXED` for the Direct-mode flush path on Linux. | Best for sustained sequential append workloads where page-cache jitter is observable. |
| **Tier 4 — io_uring elite path** | **Shipped across 0.9.4–0.9.7.** `IORING_SETUP_COOP_TASKRUN` / `SINGLE_ISSUER` / `DEFER_TASKRUN` setup flags (0.9.4, kernel ≥ 5.19 / 6.0 / 6.1); linked Write+Fsync via `IOSQE_IO_LINK` (0.9.4); `IORING_REGISTER_FILES` for fd slot-upgrade (0.9.5); `IORING_OP_WRITE_FIXED` against pre-registered AlignedBuf slots (0.9.6); `IORING_SETUP_SQPOLL` opt-in (0.9.7, `Builder::sqpoll(idle_ms)`). | 5 M – 10 M ops/s ceiling on bare-metal Linux + NVMe. Measurement pending Phase 9 bare-metal re-run. |

The Tier 4 ceiling is the same one Oracle, OceanBase, and PolarDB hit with their internal storage engines — the same Linux primitives are available to any application that uses them correctly. As of 0.9.7 every tier-4 primitive ships in code; the bench numbers documenting the win are the 0.9.8 release-prep deliverable.

---

## Direct-IO journal vs buffered (0.9.0 R-2)

The Direct-IO journal mode (opt-in via `JournalOptions::direct(true)`) trades the lock-free hot path of the buffered tiers for a sector-aligned in-memory log buffer that flushes via DMA-direct positioned writes. Architecturally analogous to InnoDB's redo-log buffer.

**Architectural differences:**

| Aspect | Buffered mode (default) | Direct-IO mode |
|--------|-------------------------|----------------|
| Append serialisation | Lock-free (atomic LSN reservation + concurrent `pwrite`) | Mutex-serialised (single buffer copy) |
| Memory copy path | User → page cache → device | User → log buffer → device (DMA) |
| Page cache pressure | Yes (writeback contention with rest of system) | No |
| Tail latency jitter | Page-cache writeback dependent | Predictable (no writeback dependency) |
| Resume after crash | Tail-truncation detected via reader | Same + partial-sector rehydration |
| File open flags | Standard read/write | `O_DIRECT` / `F_NOCACHE` / `FILE_FLAG_NO_BUFFERING` |

Choose Direct-IO when the workload is a sustained sequential WAL whose throughput is gated by device bandwidth, or when tail latency must not be perturbed by page-cache writeback. Stay with the buffered default for general-purpose use, mixed read/write workloads, and any case where the page cache is acting as a useful accelerator.

---

## Crash-safety verification (0.9.0 R-3)

[`tests/crash_journal.rs`](../tests/crash_journal.rs) ships a process-kill harness that empirically validates the journal's durability claims for both the buffered and direct-mode paths. The harness:

1. Spawns a victim subprocess that opens the journal, appends `SYNCED_COUNT = 50` records, calls `sync_through` to make those records durable, signals `BEGIN`, then continues appending more records without syncing.
2. The parent reads the `BEGIN` signal and kills the victim mid-burst (Windows `TerminateProcess` / Unix `SIGKILL`).
3. The parent reopens the journal in its own address space and scans it forward.

The harness then asserts three load-bearing invariants:

- **Durability.** All 50 synced records are present and intact. Their payloads match byte-for-byte what was written; their LSNs are monotonically increasing.
- **Tail truncation.** The reader stops cleanly at the first torn frame. The tail state is one of `CleanEnd`, `TruncatedHeader`, `TruncatedPayload`, or `ChecksumMismatch` — never `BadMagic` or `LengthOverflow` (those would indicate format-level corruption).
- **No torn-frame surface.** Records past the sync barrier may or may not be present — the journal contract makes no promise about unsynced records — but any record that the reader does surface must match its expected payload byte-for-byte. The CRC-32C check is what enforces this; if a single bit in the on-disk frame is wrong, the decoder must surface `ChecksumMismatch` rather than yielding the corrupted bytes as a "valid" record.

Both `JournalOptions::default()` (buffered/lock-free) and `JournalOptions::direct(true)` (direct-IO log buffer) pass the harness across 10 consecutive runs.

---

## 0.9.1–0.9.7 features awaiting numbered results

These features shipped between 0.9.1 and 0.9.7. They are
**production-ready** — covered by unit / integration / fuzz
tests and validated in the CI matrix — but bench numbers
isolating each feature's contribution are pending the 0.9.8
release-prep bare-metal Linux re-run.

| Feature | Release | Expected win | Bench |
|---|---|---|---|
| `JournalHandle::append_batch` vectored append | 0.9.1 | ~1.6× per-record vs `append`-in-loop on Windows NTFS; larger on Linux NVMe | needs dedicated bench |
| Hardware-accelerated CRC-32C (SSE4.2 / ARMv8 CRC) | 0.9.1 | ~10× CRC compute vs scalar fallback | covered indirectly by journal benches |
| `Builder::dispatcher_shards(N)` multi-shard batch | 0.9.3 | near-linear scaling with `N` for concurrent writers to distinct paths | `benches/concurrent_batches.rs` |
| `Batch::commit_grouped()` amortised parent-dir fsync | 0.9.3 | M/N reduction where M = distinct parent dirs, N = ops | needs dedicated bench |
| `SyncMode::Barrier` (macOS `F_BARRIERFSYNC`) | 0.9.4 | 10–100× cheaper than `F_FULLFSYNC` on Apple Silicon NVMe | macOS-only |
| io_uring elite flags (`COOP_TASKRUN` / `SINGLE_ISSUER` / `DEFER_TASKRUN`) | 0.9.4 | ~5–15% per-op reduction on supported kernels | covered by all io_uring benches |
| Linked Write+Fsync via `IOSQE_IO_LINK` | 0.9.4 | ~2× round-trip reduction on durable Direct writes | needs dedicated bench |
| Dual-buffer Direct-mode log buffer | 0.9.5 | Direct mode: single-core ceiling → multi-core scalable | needs concurrent-append bench |
| `IORING_REGISTER_FILES` slot-upgrade | 0.9.5 | ~50–200 ns per SQE | not isolable in user-space bench |
| `IORING_OP_WRITE_FIXED` Direct journal flush | 0.9.6 | Saves per-SQE kernel buffer pinning | not isolable in user-space bench |
| APFS `clonefile(2)` / ReFS `FSCTL_DUPLICATE_EXTENTS_TO_FILE` reflinks | 0.9.6 | Multi-GiB clones: seconds → microseconds | filesystem-specific bench needed |
| GroupCommit wake-stampede fix | 0.9.7 | ~5× lock-hold reduction under 100+ followers | covered by stress test |
| LSN reservation `AcqRel` → `Release` | 0.9.7 | ~0.2–0.5 µs/op on aarch64 | needs aarch64 bench |
| `Builder::sqpoll(idle_ms)` kernel-side polling | 0.9.7 | Eliminates `io_uring_enter` syscall in steady state | needs sustained-write bench |

The Phase 9 release-prep pass produces bare-metal Linux numbers
for each row in this table. Until then, the regression-budget
infrastructure (per-class baselines, ≤ 5–25% strictness gates
in [`baselines.json`](../benches/baselines.json)) catches any
regression even on the older bench shapes.

---

## 1.1.0 — capability cache + backend observability benches

Two new benchmarks track the cost of the 1.1.0 public observability surface.

### `cargo bench --bench capability_access`

Measures three latencies that callers depend on:

| Bench | Floor target | What it measures |
|---|---|---|
| `capability_access/capabilities_warm` | well under 1 µs | Steady-state `capabilities()` call after the first; pure `OnceLock` pointer load. |
| `capability_access/probe_fresh` | 50&ndash;200 ms | `probe_capabilities_fresh()` — full sysfs + procfs walk + TOML serialise + atomic-replace rewrite. |
| `pci_address/to_canonical` | < 200 ns | `PciAddress::to_canonical()` &mdash; `format!` of four hex fields. |
| `pci_address/parse_four_segment` | < 200 ns | `PciAddress::parse("0000:1f:03.2")` &mdash; three `split_once` + `from_str_radix` calls. |

These exist primarily to catch regressions; the warm path's < 1 µs floor is what makes per-second health-check polling safe. Any future change that pushes the warm call above ~1 µs is a 1.1.0 contract regression and must be flagged.

### `cargo bench --bench backend_accessors`

Measures the cost of the three `JournalHandle` observability accessors:

| Bench | Floor target | What it measures |
|---|---|---|
| `backend_accessors/backend_kind` | well under 50 ns | `JournalHandle::backend_kind()` &mdash; plain field read + cfg-gated `OnceLock::get()` peek. |
| `backend_accessors/backend_health` | well under 50 ns | `JournalHandle::backend_health()` &mdash; classification + `JournalBackendHealth::empty()` (const fn). |
| `backend_accessors/backend_info` | < 1 µs | `JournalHandle::backend_info()` &mdash; allocates a `String` for `selection_reason` and captures `SystemTime::now()`. |

The kind + health accessors are sub-50-ns by construction (no allocation, no syscall); the info accessor allocates one short string per call. Per-second monitoring of all three is comfortably under any sensible budget.

---

## See also

- [`PERFORMANCE.md`](PERFORMANCE.md) &mdash; tuning knobs and what each one moves.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) &mdash; the substrate-selection and dispatcher layering that the benches measure.
- [`.dev/DECISIONS-0.7.0.md`](../.dev/DECISIONS-0.7.0.md) &mdash; locked decisions D-5 (relative tail target), D-8 (1.46&times; WSL measurement), and the hybrid strictness rationale.
