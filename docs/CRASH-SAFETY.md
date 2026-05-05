<h1 align="center">
  <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
  <br>
  <code>FSYS &plus; RUST</code>
  <br>
  CRASH SAFETY
</h1>

Every write-path method in fsys uses an atomic temp-file +
rename pattern. The contract is the same across methods; only
the primitives that achieve durability differ.

## The contract

For every public write API that produces a final file at `path`
(`write`, `write_copy`, `write_batch`, `Batch::commit`):

> **Invariant.** At every observable point in time, the file at
> `path` is either entirely the old payload (or absent) or
> entirely the new payload. There is no observable state where
> the file contains partial data, mixed bytes from old and new,
> or torn writes.

This holds across:
- Process kill at any point during the write.
- OS panic / power loss after the rename completes (data is
  durable on stable storage by then).
- Concurrent readers — they see either the old or the new
  payload, never a mix.

## What happens during a write

```
1. Open temp file (`<path>.fsys-tmp-<n>`)
2. Write data (with the configured durability primitive)
3. Apply preserved metadata to the temp (write_copy only)
4. Atomic rename: temp → path
5. Sync parent directory (Linux/macOS; no-op on Windows)
```

If a crash occurs:
- **Before step 4:** `path` is unchanged (or absent). The temp
  file may remain on disk; it is safe to delete.
- **During step 4:** the kernel's rename is atomic — either the
  rename completed (target is new) or it didn't (target is old).
- **After step 4 but before step 5:** target is new. Some
  filesystems may lose the rename's metadata commit on a power
  loss without step 5; the safe assumption is "new content,
  durable" because most journaling filesystems already commit
  the rename on the journal flush before returning.

## Per-method primitives

### `Method::Sync`

| Platform | Primitive |
|---|---|
| Linux | `pwrite` + `fsync` |
| macOS | `pwrite` + `fcntl(F_FULLFSYNC)` |
| Windows | `WriteFile` + `FlushFileBuffers` |

`fsys::primitive::FSYNC` (Linux/Windows) /
`fsys::primitive::F_FULLFSYNC` (macOS).

### `Method::Data`

Same as `Sync` except Linux uses `fdatasync` instead of `fsync`,
which skips metadata-only flushes (e.g. atime updates) for ~20%
lower latency on consumer NVMe.

`fsys::primitive::FDATASYNC` (Linux); falls back to Sync's
primitive on macOS/Windows.

### `Method::Direct`

| Platform | Primitive |
|---|---|
| Linux + io_uring + NVMe passthrough | `io_uring + NVMe FLUSH` |
| Linux + io_uring | `io_uring + fdatasync` |
| Linux fallback | `O_DIRECT + pwrite + fdatasync` |
| macOS | `F_NOCACHE + F_FULLFSYNC` |
| Windows + NVMe IOCTL | `FILE_FLAG_WRITE_THROUGH + NVMe IOCTL` |
| Windows fallback | `FILE_FLAG_WRITE_THROUGH` |

The atomic-replace contract holds in every case. See
[`fsys::primitive`](https://docs.rs/fsys/latest/fsys/primitive/)
for the full list of canonical strings.

### `Method::Mmap`

`mmap` + `msync(MS_SYNC)` on Unix; `MapViewOfFile` +
`FlushViewOfFile` on Windows. Sub-page payloads, zero-length
writes, and non-regular files transparently fall back to
`Method::Sync` (the kernel cannot `msync` a sub-page region
durably). The fallback updates `active_method()` and is
**permanent for the lifetime of the handle** — once Mmap falls
back for a handle, it stays fallen-back.

## Crash-test harness

`tests/crash_*.rs` validates the contract under three kill modes:

- **`PreSyscall`** — kill before any data write. File must be
  entirely old (or absent).
- **`MidSyscall`** — kill during the syscall. File must NOT be
  torn — it must be either entirely old or entirely new.
- **`PostSyscall`** — kill after rename. File must be entirely
  new.

Per the D-4 protocol from `0.4.0`, every crash-safety change runs
the test 100× pre-merge to catch flakes. The 0.5.0 / 0.5.1
crash-test runs are documented in
`.dev/DECISIONS-0.5.0.md` and `.dev/DECISIONS-0.6.0.md`.

## Non-write APIs

These APIs do NOT use the atomic-replace pattern (and do not
guarantee atomicity):

- `Handle::append` — extends the file in-place. Crash mid-append
  may leave a file with fewer-than-expected bytes; the file is
  never corrupted, just shorter.
- `Handle::write_at` — writes at an arbitrary offset. Crash
  mid-write leaves either the old bytes or the new bytes at that
  offset; on filesystems with sector-granular atomicity (most),
  individual sector writes are atomic.
- `Handle::truncate` — `set_len` is atomic at the kernel level.
- `Handle::rename` — `rename(2)` / `MoveFileExW` are atomic
  within a filesystem.
- `Handle::delete` / `Handle::rmdir*` — `unlink(2)` / `rmdir(2)`
  / `DeleteFile` are atomic.
