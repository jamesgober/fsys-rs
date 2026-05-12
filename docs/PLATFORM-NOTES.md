<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  PLATFORM NOTES
</h1>

fsys ships on Linux, macOS, and Windows. Behavior is
platform-honest — every divergence is documented here and
reflected in the per-method docs.

## Linux

### Primary performance target

Linux is the primary perf target. The fastest paths
(`io_uring`, NVMe passthrough flush) are Linux-only.

### `Method::Direct` flow

1. `open(O_DIRECT)` — direct IO bypassing the page cache. tmpfs
   and some FUSE backends reject `O_DIRECT`; on rejection, fsys
   transparently falls back to `Method::Data` and updates
   `active_method()`.
2. Write via `io_uring` (when available — kernel ≥ 5.1) or
   `pwrite(2)` fallback.
3. Durability: NVMe passthrough flush via `NVME_IOCTL_IO_CMD`
   ioctl when the device is NVMe and the process has raw NVMe
   access (`CAP_SYS_ADMIN` or membership in the `disk` group).
   Otherwise `fdatasync(2)` via io_uring or syscall.

### NVMe passthrough requirements

- Linux kernel ≥ 4.12 (for `NVME_IOCTL_IO_CMD`).
- The fd's underlying block device must be NVMe (not SATA SSD,
  not HDD, not loop device, not tmpfs).
- The process must be able to open `/dev/nvmeX` (the character
  device) with `O_RDWR`. Typical privilege paths:
  - `CAP_SYS_ADMIN`.
  - Member of the `disk` group (configurable per distro).
- `FSYS_DISABLE_NVME_PASSTHROUGH=1` env override forces the
  fallback path. **Testing aid only** — production callers who
  want to disable NVMe passthrough should explicitly use
  `Method::Data` or `Method::Sync`.

### io_uring availability

- Kernel ≥ 5.1.
- `io_uring_setup(2)` not blocked by SECCOMP / AppArmor /
  container restrictions.
- Some hardened distros (e.g. the recent default on Google's
  internal Linux) disable `io_uring_setup` system-wide. fsys
  detects this and falls back; the failure is observable via
  `Handle::active_durability_primitive()` returning
  `O_DIRECT + pwrite + fdatasync` instead of `io_uring + …`.

### io_uring elite flags (0.9.4+)

When io_uring is available, fsys runs a one-time
process-cached probe at handle construction to test which
elite setup flags the kernel supports:

| Flag | Kernel | Effect |
|---|---|---|
| `IORING_SETUP_COOP_TASKRUN` | ≥ 5.19 | Defer completion task work until convenient (reduces IPIs). Pure perf hint. |
| `IORING_SETUP_SINGLE_ISSUER` | ≥ 6.0 | Kernel-enforced same-task submission. Sync ring only — async substrate disabled because tokio migrates tasks. |
| `IORING_SETUP_DEFER_TASKRUN` | ≥ 6.1 | Requires `SINGLE_ISSUER`. Sync ring only — needs explicit `io_uring_enter(GETEVENTS)` driving which the async eventfd loop doesn't do. |
| `IORING_REGISTER_FILES` (0.9.5) | ≥ 5.1 | Pre-register fd-table slots; per-op submissions use `IOSQE_FIXED_FILE`, saving per-SQE kernel-side fd validation. |
| `IORING_OP_WRITE_FIXED` (0.9.6) | ≥ 5.6 | Pre-register buffer slots; writes against fixed slots avoid per-SQE kernel buffer pinning. Used by the journal Direct-mode flush path. |
| `IORING_REGISTER_BUFFERS` (0.9.6) | ≥ 5.1 | Companion to `WRITE_FIXED` — registers the `AlignedBuf` slots. |
| `IORING_SETUP_SQPOLL` (0.9.7, opt-in) | ≥ 5.13 | Kernel-side polling thread drains the SQ without syscalls. Opt-in via `Builder::sqpoll(idle_ms)`. Requires `CAP_SYS_NICE` on kernels < 5.13. |

All flags downgrade gracefully on older kernels; unsupported
flags are silently omitted and the ring builds with whatever
the kernel does support.

### NVMe atomic-write unit probe (0.9.4)

`Handle::atomic_write_unit() -> Option<u32>` probes the NVMe
Identify Namespace command (NAWUN / NAWUPF fields) and returns
the drive's guaranteed torn-write-free write size, when known.
Databases on guaranteeing drives can safely skip torn-write
detection on writes ≤ that size.

The probe runs once at first Direct op via the io_uring
passthrough path; result is cached for the handle's lifetime.
Drives that don't expose NAWUN return `None`; non-NVMe storage
returns `None`.

### OS-version + page-size probes (0.9.6)

- Real OS-version probe via `sysctlbyname` (macOS) /
  `RtlGetVersion` (Windows) / `uname -r` (Linux). Before 0.9.6
  these returned `"unknown"` stubs on macOS / Windows.
- Real page-size probe via `sysconf(_SC_PAGESIZE)` (Unix) /
  `GetSystemInfo` (Windows). Before 0.9.6 the value was a
  build-time constant.

Probe results live in `fsys::os::info()` /
`fsys::hardware::info()`.

## macOS

### `Method::Direct` flow

1. `open` (no O_DIRECT — macOS doesn't have it).
2. `fcntl(F_NOCACHE)` to disable page cache for the fd.
3. Write via `pwrite(2)`.
4. Durability via `fcntl(F_FULLFSYNC)` — regular `fsync(2)` on
   macOS does NOT actually flush to media (it returns when the
   page cache is flushed to the device's volatile cache).
   `F_FULLFSYNC` is the macOS primitive that actually waits for
   media.

### NVMe passthrough

**Not supported.** macOS does not expose the necessary primitives
in mainstream APIs. Apple's IOKit can probe SMART data but does
not provide a path for issuing raw NVMe commands from userspace.
`Method::Direct` on macOS uses `F_NOCACHE + F_FULLFSYNC`.

This is a permanent restriction unless Apple ships a public API.
Filed as F-12 (possibly never).

### `Method::Data`

`fdatasync(2)` is not available on macOS. `Method::Data`
transparently falls back to `Method::Sync`'s primitive
(`F_FULLFSYNC`).

### `SyncMode::Barrier` for journals (0.9.4)

Journal users can opt into `JournalOptions::sync_mode(SyncMode::Barrier)`
to use Apple's `F_BARRIERFSYNC` instead of `F_FULLFSYNC`. The
barrier primitive is **10–100× cheaper** than `F_FULLFSYNC` on
Apple Silicon NVMe because it returns when writes have reached
the device's volatile cache without waiting for the cache flush
to media.

**Crash safety contract.** `F_BARRIERFSYNC` is crash-safe **only**
on PLP-equipped drives (the capacitor backs the cache through
power loss), **or** under explicit eventual-`SyncMode::Full`-sync
discipline (a periodic `Full` sync at checkpoint boundaries).
On non-PLP consumer NVMe without checkpoint discipline, a power
loss between `Barrier` sync and the next cache flush can lose
the most recent records.

`SyncMode::Full` (default) is universally crash-safe and remains
the right choice when in doubt.

### APFS `clonefile(2)` reflink (0.9.6)

`Handle::copy(src, dst)` uses `clonefile(2)` on APFS for instant
copy-on-write semantics. Multi-GiB file clones drop from seconds
to microseconds. Falls back to `std::fs::copy` cleanly on:
- HFS+ (no clonefile support)
- Cross-volume copies (`EXDEV`)
- Existing destinations (`EEXIST`)
- Permission denials (`EACCES`)

The fallback path is observable indirectly via wall-clock time
on multi-GiB files (clonefile is < 10 ms; fallback scales with
file size).

## Windows

### `Method::Direct` flow

1. `CreateFileW` with `FILE_FLAG_NO_BUFFERING |
   FILE_FLAG_WRITE_THROUGH`. `WRITE_THROUGH` makes every write
   durable on return — no separate flush call needed.
2. Aligned `WriteFile` calls (alignment matches sector size).

### NVMe passthrough requirements

- Windows 10 1903+ (for `IOCTL_STORAGE_PROTOCOL_COMMAND`).
- The process must have **admin privileges** to open the volume
  with `GENERIC_READ | GENERIC_WRITE`. Most non-elevated
  processes hit `ERROR_ACCESS_DENIED`; fsys falls back to
  `WRITE_THROUGH` and surfaces the fallback via
  `active_durability_primitive()`.
- `FSYS_DISABLE_NVME_PASSTHROUGH=1` env override forces fallback.

### Path-length notes

Windows's classic `MAX_PATH = 260` constrains the *full* path
including parent directories. fsys does not opt into the
extended-path namespace (`\\?\` prefix) automatically — pass
extended paths explicitly if you need > 260-character paths.

### `Method::Data` falls back to `Method::Sync`

Windows has no direct equivalent of `fdatasync`. `Method::Data`
on Windows uses `FlushFileBuffers` (the Sync primitive) — same
flush, no metadata-skip optimisation.

### ReFS `FSCTL_DUPLICATE_EXTENTS_TO_FILE` reflink (0.9.6)

`Handle::copy(src, dst)` uses ReFS's reflink semantics via
`FSCTL_DUPLICATE_EXTENTS_TO_FILE` when both source and
destination live on the same ReFS volume. Same instant-clone
behavior as APFS `clonefile` on macOS. Falls back to
`std::fs::copy` cleanly on:
- NTFS (no reflink support)
- Cross-volume copies
- Permission denials
- Pre-Windows-Server-2016 systems

The implementation uses raw `DeviceIoControl` against a
fresh-destination handle with `FILE_SHARE_DELETE`. Verified
against the FSCTL contract.

## Cross-platform sparse-file primitives (0.9.5)

`Handle::punch_hole(path, offset, len)` and
`Handle::write_zeros(path, offset, len)` expose cross-platform
sparse-file APIs:

| Platform | Primitive |
|---|---|
| Linux | `fallocate(FALLOC_FL_PUNCH_HOLE | FL_KEEP_SIZE)` for `punch_hole`; `fallocate(FL_ZERO_RANGE)` for `write_zeros` |
| macOS | `fcntl(F_PUNCHHOLE)` for both |
| Windows | `FSCTL_SET_ZERO_DATA` for both |

Used as the WAL-trim primitive in databases that give back
consumed log segments without touching the page cache. All
three primitives are kernel-atomic.

## Cross-platform fallback ladder

When a method's primary primitive isn't available, fsys falls
back **transparently and observably**:

```
Direct (Linux)   →  if O_DIRECT fails     →  Data (fdatasync)
                                          →  if Data fails    →  Sync (fsync)

Direct (macOS)   →  if F_NOCACHE fails    →  Sync (F_FULLFSYNC)

Direct (Windows) →  if NO_BUFFERING fails →  Sync (FlushFileBuffers)
```

`Handle::active_method()` always returns the truth — read it
back to know which primitive actually ran.

`Handle::active_durability_primitive()` (0.6.0+) returns the
canonical primitive string for finer-grained observability.

## Filesystem caveats

Some filesystems will reject `O_DIRECT` / `FILE_FLAG_NO_BUFFERING`:

| Filesystem | O_DIRECT | NO_BUFFERING | Reflink | fsys behavior |
|---|---|---|---|---|
| ext4, XFS, btrfs | yes | n/a | varies | Direct path used |
| tmpfs | NO | n/a | NO | Falls back to Data on Linux |
| FUSE (most) | NO | n/a | NO | Falls back to Data on Linux |
| FAT32 | varies | varies | NO | May fall back to Sync; no reflink |
| exFAT | varies | varies | NO | May fall back to Sync; no reflink |
| NTFS | n/a | yes | NO | Direct path used on Windows; `copy` falls back to `std::fs::copy` |
| ReFS | n/a | yes | **YES** (0.9.6) | `copy` uses `FSCTL_DUPLICATE_EXTENTS_TO_FILE` for instant clone |
| APFS | n/a | n/a (uses F_NOCACHE) | **YES** (0.9.6) | F_NOCACHE used on macOS; `copy` uses `clonefile(2)` for instant clone |
| HFS+ | n/a | n/a (uses F_NOCACHE) | NO | F_NOCACHE used on macOS; `copy` falls back to `std::fs::copy` |

When in doubt, run a write and check `active_method()` /
`active_durability_primitive()`.
