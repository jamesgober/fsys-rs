<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  DURABILITY METHODS
</h1>

`Method` is the choice of *how* fsys makes your bytes durable.
Pick the cheapest method that satisfies your durability requirement.

## The matrix

| Method | Linux | macOS | Windows | Cost (consumer NVMe) | Use when |
|---|---|---|---|---|---|
| `Sync` | `fsync(2)` | `fcntl(F_FULLFSYNC)` | `FlushFileBuffers` | ~1–10 ms | universal correctness floor |
| `Data` | `fdatasync(2)` | falls back to `Sync` | falls back to `Sync` | ~500 µs–5 ms | data-only durability, no metadata |
| `Mmap` | `mmap` + `msync(MS_SYNC)` | `mmap` + `msync` | `MapViewOfFile` + `FlushViewOfFile` | size-dependent | read-heavy random access |
| `Direct` | `O_DIRECT` + io_uring (+ NVMe FLUSH on capable HW) | `F_NOCACHE` + `F_FULLFSYNC` | `FILE_FLAG_WRITE_THROUGH` (+ NVMe IOCTL on capable HW) | < 100 µs target | append-heavy or cache-bypass writes |
| `Journal` | reserved variant — no committed implementation | reserved variant | reserved variant | n/a | reserved enum slot only; see note below |
| `Auto` | hardware-aware | hardware-aware | hardware-aware | varies | "pick something sensible" |

> **Note on `Method::Journal`.** This enum variant is a forward-compatibility placeholder reserved at 0.7.0 and intentionally not implemented. Append-only / write-ahead-log workloads should use the dedicated [journal substrate](API.md#journal-substrate) shipped in 0.9.0 — opened via [`Handle::journal`] / [`Handle::journal_with`], surfaced through [`JournalHandle`], and entirely independent of the `Method` enum. The journal substrate is a structurally different primitive (open-once log file with explicit LSN reservation and group-commit fsync) rather than a per-write durability strategy, which is why it lives outside the `Method` taxonomy.

## How to pick

1. **You don't know what you need:** use `Method::Auto`. It probes
   the hardware and picks the fastest method that's safe for the
   detected drive class.

2. **You have a clear correctness requirement and want the
   simplest contract:** `Method::Sync`. Slowest, universally
   available, semantics match what every Unix programmer expects
   from `fsync`.

3. **You care about metadata-vs-data distinction (e.g. you control
   inode atime/mtime separately):** `Method::Data`. Linux-only
   speedup; falls back to `Sync` elsewhere — observable via
   `Handle::active_method()`.

4. **You're doing a lot of small random writes and the dataset
   fits in memory:** `Method::Mmap`. Sub-page payloads
   transparently fall back to `Sync` (the kernel can't `msync` a
   sub-page region durably).

5. **You're doing append-heavy writes on an NVMe SSD with PLP
   (Power Loss Protection):** `Method::Direct`. Best-case latency;
   on Linux uses io_uring + (if available) NVMe passthrough flush.

## What `Auto` picks

Resolved once at handle construction. The decision is deterministic
given the hardware probe.

```
Linux + io_uring + NVMe + NVMe passthrough capability  →  Direct
Linux + io_uring + NVMe                                →  Direct (fdatasync flush)
Linux + NVMe (no io_uring)                             →  Data
Linux + non-NVMe SSD                                   →  Data
Linux + HDD or unknown                                 →  Sync
macOS + NVMe                                           →  Direct (F_NOCACHE+F_FULLFSYNC)
macOS + non-NVMe                                       →  Sync
Windows + NVMe + IOCTL access                          →  Direct (NVMe IOCTL)
Windows + NVMe (no IOCTL access)                       →  Direct (WRITE_THROUGH)
Windows + non-NVMe                                     →  Sync
Anything that fails to probe                           →  Sync (universal safety)
```

`Auto` never falls through to user code at runtime — the choice is
locked at handle construction. Subsequent runtime fallbacks (e.g.
`O_DIRECT` rejected by tmpfs) are observable via
`Handle::active_method()`.

## Observing fallbacks

`Handle::active_method()` returns the method actually in effect.
If you requested `Method::Direct` and the filesystem rejected
`O_DIRECT` at open time, the handle's active method downgrades to
`Method::Data`. You can read it back at any time.

`Handle::active_durability_primitive()` (new in 0.6.0) returns the
canonical *primitive* string — e.g. `"io_uring + NVMe FLUSH"`,
`"fdatasync"`, `"FILE_FLAG_WRITE_THROUGH"`. Match against the
constants in [`fsys::primitive`](https://docs.rs/fsys/latest/fsys/primitive/)
to avoid string-typo bugs.

## Crash safety

Every method's write path uses an atomic temp-file + rename
pattern. The target file is either entirely the old payload (kill
before rename) or entirely the new payload (kill after rename).
Never torn. See [`CRASH-SAFETY.md`](CRASH-SAFETY.md) for the full
contract per method.
