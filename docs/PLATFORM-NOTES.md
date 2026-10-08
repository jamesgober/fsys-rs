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
| `IORING_REGISTER_FILES` (0.9.5, removed in 1.1.1) | n/a | No longer used. The slot cache was keyed by fd number and sent writes to the wrong file once a closed fd number was reused; every SQE now carries the caller's raw fd. |
| `IORING_OP_WRITE_FIXED` (0.9.6) | ≥ 5.6 | Pre-register buffer slots; writes against fixed slots avoid per-SQE kernel buffer pinning. Used by the journal Direct-mode flush path. |
| `IORING_REGISTER_BUFFERS` (0.9.6) | ≥ 5.1 | Companion to `WRITE_FIXED` — registers the `AlignedBuf` slots. |
| `IORING_SETUP_SQPOLL` (0.9.7, opt-in) | ≥ 5.13 | Kernel-side polling thread drains the SQ without syscalls. Opt-in via `Builder::sqpoll(idle_ms)`. Requires `CAP_SYS_NICE` on kernels < 5.13. Before 5.11 an SQPOLL ring only accepts registered files, so with raw-fd SQEs each op fails and the Direct path falls back to `pwrite`. |

All flags downgrade gracefully on older kernels; unsupported
flags are silently omitted and the ring builds with whatever
the kernel does support.

### NVMe atomic-write unit probe (0.9.4)

`Handle::atomic_write_unit() -> Option<u32>` probes the NVMe
Identify Namespace command (NAWUN / NAWUPF fields) and returns
the drive's guaranteed torn-write-free write size, when known.
Databases on guaranteeing drives can safely skip torn-write
detection on writes ≤ that size.

The probe is part of the process-wide hardware drive probe: it
runs once, against the NVMe controller (`/dev/nvmeX`, which needs
`CAP_SYS_ADMIN` or the `disk` group) behind the drive that holds
the process's current working directory (see
[Hardware probe scope](#hardware-probe-scope)). Drives that don't
expose NAWUN, non-NVMe storage, and unprivileged processes get
`None`. Before 1.1.1 the controller name was derived incorrectly
and the probe always returned `None`.

### OS-version + page-size probes (0.9.6)

- Real OS-version probe via `sysctlbyname("kern.osproductversion")`
  (macOS) / `RtlGetVersion` (Windows) /
  `/proc/sys/kernel/osrelease` (Linux). Before 0.9.6 these
  returned `"unknown"` stubs on macOS / Windows.
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
4. Durability via `fcntl(F_FULLFSYNC)` after the write: regular
   `fsync(2)` on macOS does NOT actually flush to media (it
   returns when the page cache is flushed to the device's volatile
   cache). `F_FULLFSYNC` is the macOS primitive that actually waits
   for media. `F_NOCACHE` alone only bypasses the buffer cache; it
   does not flush the drive.

### `F_FULLFSYNC` fallback

Some file systems (certain SMB / NFS / FUSE mounts) do not
implement `F_FULLFSYNC` and reject it with `ENOTSUP`,
`EOPNOTSUPP` or `EINVAL`. Since 1.1.1 fsys then falls back to
`fsync(2)`, the strongest flush such a mount offers, instead of
failing the write. The same applies to the directory sync after
an atomic replace. Any other error is returned.

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
   Transfers of 4 GiB or more are split into 2 GiB chunks (a
   multiple of every sector size), since `WriteFile` / `ReadFile`
   take a 32-bit length.

### NVMe passthrough requirements

- Windows 10 1903+ (for `IOCTL_STORAGE_PROTOCOL_COMMAND`).
- The process must have **admin privileges** to open the volume
  with `GENERIC_READ | GENERIC_WRITE`. Most non-elevated
  processes hit `ERROR_ACCESS_DENIED`; fsys falls back to
  `WRITE_THROUGH` and surfaces the fallback via
  `active_durability_primitive()`.
- The storage driver must execute a standard NVMe FLUSH sent
  through that IOCTL. Microsoft documents the IOCTL for
  vendor-specific commands, and the inbox StorNVMe driver is
  expected to reject FLUSH; on such systems the probe fails and
  fsys stays on `WRITE_THROUGH`. The capability probe sends a
  real FLUSH (all namespaces) and requires both a successful
  `DeviceIoControl` and `STORAGE_PROTOCOL_STATUS_SUCCESS`, so a
  reported NVMe-flush primitive means a flush was acknowledged.
- `FSYS_DISABLE_NVME_PASSTHROUGH=1` env override forces fallback.

### `SyncMode::Barrier` for journals

Windows has no barrier-only flush. `platform::sync_barrier` checks
whether the journal's handle was opened with
`FILE_FLAG_WRITE_THROUGH` (`NtQueryInformationFile`,
`FileModeInformation`):

- write-through handle (Direct journal on a volume that accepted
  `FILE_FLAG_NO_BUFFERING`): every write was already durable when
  `WriteFile` returned, so the barrier returns immediately;
- any other handle (the default buffered journal, and the Direct
  journal's buffered fallback after `ERROR_INVALID_PARAMETER`):
  `FlushFileBuffers`, the same cost as `SyncMode::Full`.

Before 1.1.1 the barrier was a no-op on every Windows handle, so a
buffered journal could report records as committed while they
were still in the OS cache.

### Rename and directory durability

`MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`
performs the atomic replace, but `MOVEFILE_WRITE_THROUGH` only
waits for the flush when the move is carried out as copy + delete
(across volumes). For a same-volume rename, fsys then opens the
parent directory with `FILE_FLAG_BACKUP_SEMANTICS` and
`FILE_WRITE_DATA` access (no administrator rights needed) and
calls `FlushFileBuffers` on it (`platform::sync_parent_dir`).
File systems that cannot flush a directory handle
(`ERROR_INVALID_FUNCTION`, `ERROR_NOT_SUPPORTED`,
`ERROR_INVALID_PARAMETER`) are treated as success; before 1.1.1
this step was skipped on every volume.

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
- NTFS and other volumes without block cloning (checked through
  `FILE_SUPPORTS_BLOCK_REFCOUNTING` before anything is created)
- Cross-volume copies
- An existing destination
- Permission denials
- Pre-Windows-Server-2016 systems

The destination is created fresh (`CREATE_NEW`, `FILE_SHARE_DELETE`),
sized to the source, marked sparse when the source is, and cloned
in cluster-aligned ranges below 4 GiB (the final range is rounded
up past EOF, which ReFS accepts at end of file). If any step fails
after the destination was created, it is deleted before the
`std::fs::copy` fallback runs. The ReFS path has unit tests for the
range arithmetic but has not been exercised on a ReFS volume in
fsys's own test runs.

## Cross-platform sparse-file primitives (0.9.5)

`Handle::punch_hole(path, offset, len)` and
`Handle::write_zeros(path, offset, len)` expose cross-platform
sparse-file APIs:

| Platform | `punch_hole` | `write_zeros` |
|---|---|---|
| Linux | `fallocate(FALLOC_FL_PUNCH_HOLE \| FL_KEEP_SIZE)` | `fallocate(FL_ZERO_RANGE \| FL_KEEP_SIZE)`; positioned zero writes where the file system returns `EOPNOTSUPP` (tmpfs, most FUSE) |
| macOS | `fcntl(F_PUNCHHOLE)` on the whole file-system blocks in the range (clipped to EOF); unaligned head / tail overwritten with zeros | positioned zero writes |
| Windows | `FSCTL_SET_ZERO_DATA` | positioned zero writes |

Used as the WAL-trim primitive in databases that give back
consumed log segments without touching the page cache. After
either call every byte of the range reads as zero. Storage is only
returned where the primitive actually deallocates: NTFS releases
clusters for `FSCTL_SET_ZERO_DATA` only on sparse files (fsys does
not mark files sparse), and the macOS edge blocks stay allocated.
The positioned-write paths are not atomic and, unlike
`FL_KEEP_SIZE`, extend the file when the range runs past EOF.

## Preallocation

`JournalHandle::preallocate(offset, len)` reserves space for
`[0, offset + len)` without changing the logical file size:

| Platform | Primitive |
|---|---|
| Linux | `fallocate(FALLOC_FL_KEEP_SIZE)`; `posix_fallocate` where unsupported |
| macOS | `fcntl(F_PREALLOCATE, F_PEOFPOSMODE)` for only the shortfall between `offset + len` and the bytes already allocated (`st_blocks * 512`) |
| Windows | `SetFileInformationByHandle(FileAllocationInfo)`, only when `offset + len` exceeds the current `AllocationSize` |

On macOS and Windows a request that is already covered does
nothing, so repeated or smaller calls never grow (macOS) or shrink
(Windows) an earlier reservation; both did before 1.1.1.

## Hardware probe scope

`fsys::hardware::drive()` / `info()`, `Handle::plp_status()`,
`Handle::is_plp_protected()` and `Handle::atomic_write_unit()`
describe the drive that holds the **process's current working
directory** at the time of the first probe. They do not look at a
handle's root or at any particular file, and the answer is cached
for the life of the process. If your data lives on a different
drive, start the process with its working directory on the data
volume, or treat these values as unknown.

PLP detection itself is a vendor / model lookup table of known
power-loss-protected enterprise drives (Linux sysfs, Windows
`IOCTL_STORAGE_QUERY_PROPERTY`, which works without administrator
rights since 1.1.1). It reports `Yes` on a hit and `Unknown`
otherwise, never `No`; macOS always reports `Unknown`. Drive kind
is classified on Linux only.

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
