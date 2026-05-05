# Platform Notes

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

| Filesystem | O_DIRECT | NO_BUFFERING | fsys behavior |
|---|---|---|---|
| ext4, XFS, btrfs | yes | n/a | Direct path used |
| tmpfs | NO | n/a | Falls back to Data on Linux |
| FUSE (most) | NO | n/a | Falls back to Data on Linux |
| FAT32 | varies | varies | May fall back to Sync |
| exFAT | varies | varies | May fall back to Sync |
| NTFS | n/a | yes | Direct path used on Windows |
| APFS / HFS+ | n/a | n/a (uses F_NOCACHE) | F_NOCACHE used on macOS |

When in doubt, run a write and check `active_method()` /
`active_durability_primitive()`.
