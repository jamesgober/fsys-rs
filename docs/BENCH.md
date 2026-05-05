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
- `linux-bare-nvme` &mdash; Linux on bare metal with NVMe (target for `0.8.0` certification).
- `windows-ntfs-nvme` &mdash; Windows + NTFS on NVMe.
- `macos-apfs-nvme` &mdash; macOS + APFS on Apple-silicon NVMe.

Numbers from one class are not directly comparable to another; the regression check happens within-class only.

## 0.7.0 native-vs-`spawn_blocking` measurement

[`benches/async_native_vs_blocking.rs`](../benches/async_native_vs_blocking.rs) is the A/B harness for the new native io_uring async substrate. It runs the same async-write workload twice &mdash; once with `FSYS_DISABLE_NATIVE_ASYNC=1` (forces the `spawn_blocking` fallback), once without (allows the native path to engage on Linux + `Method::Direct`).

The Linux substrate is required to be at least 1.1&times; faster than `spawn_blocking` to be considered functional (anything below means something went wrong with the ring construction or completion driver). Anything above 1.1&times; is a real win.

**Measured to date:** 1.46&times; on `wsl-ext4-nvme` at 4 KiB writes (`0.7.0` checkpoint G). Bare-metal Linux + NVMe is expected to be 2&times;+ but has not yet been formalised &mdash; that measurement is on the `0.8.0` checklist.

## Tail-validation harness

[`benches/tail_validation.rs`](../benches/tail_validation.rs) is a sample-and-percentile harness independent of Criterion. It captures 100 K samples per workload, computes p50/p95/p99/p99.9, and asserts the relative-tail rule (p99.9 &le; 10&times; p50). Useful for catching long-tail GC-style stalls that average-throughput benches hide.

## See also

- [`PERFORMANCE.md`](PERFORMANCE.md) &mdash; tuning knobs and what each one moves.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) &mdash; the substrate-selection and dispatcher layering that the benches measure.
- [`.dev/DECISIONS-0.7.0.md`](../.dev/DECISIONS-0.7.0.md) &mdash; locked decisions D-5 (relative tail target), D-8 (1.46&times; WSL measurement), and the hybrid strictness rationale.
