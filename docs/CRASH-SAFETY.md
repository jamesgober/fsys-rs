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

## Journal substrate durability (0.9.0)

The journal does **not** use atomic-replace. The contract is
explicit LSN-based durability: every `append` adds bytes to the
file without an `fsync`; `sync_through(lsn)` is the durability
barrier.

> **Journal invariant.** After `sync_through(lsn)` returns
> successfully, every byte from file offset 0 through `lsn.0 - 1`
> is on stable storage. Bytes past `lsn.0` may or may not be
> durable; the contract makes no promise about them.

For a database WAL pattern: append many records, take note of
the last LSN, call `sync_through(last_lsn)`, then commit the
transaction. The fsync syscall happens once per transaction, not
once per record — that's where the 100-700× speedup over
atomic-replace comes from.

### Tail-truncation taxonomy

After a crash, the journal's append-only nature means the file's
last record may be torn. The 5-state taxonomy classifies what
the reader sees at the journal tail:

| `JournalTailState` | Meaning | Recoverable? |
|---|---|---|
| `CleanEnd` | File ended on a frame boundary, or only zero bytes (Direct-IO padding, preallocated space) follow the last frame. `position()` is the end of the last frame. | n/a |
| `TruncatedHeader` | Last frame's 8-byte header is partial, or a frame position holds zero bytes that are not a 1.1.0 writer gap (a reservation that was never written). Truncate at `position()`. | **Yes** |
| `TruncatedPayload` | Last frame's payload was cut mid-write (its declared length runs past the end of the file). Truncate at frame start. | **Yes** |
| `ChecksumMismatch` | Frame decoded but CRC-32C check failed. Truncate at frame start. | **Yes** |
| `BadMagic` | A frame starts with a non-zero byte but its magic+version doesn't match. Format-level corruption; opening the journal fails with `Error::Io(InvalidData)`. | **No**: operator intervention required |
| `LengthOverflow` | Frame's declared length exceeds the 256 MiB limit. Same as `BadMagic`. | **No** |

For recoverable tail states, the recovery procedure is:

1. Open the journal via `JournalReader::open(path)`.
2. Iterate; the iterator stops at the first torn frame.
3. `reader.position()` returns the byte offset of the torn frame's start.
4. `reader.tail_state()` returns which of the five states.
5. Truncate the file to `reader.position()`, then reopen via
   `Handle::journal` to continue appending.

Since 1.1.1 `Handle::journal` / `Handle::journal_with` run steps
1-5 themselves in both modes: the open scans to the end of the
last clean frame, truncates the file there (Direct-IO keeps the
rest of that sector and rewrites it on the next flush) and
resumes appending at that LSN. Before 1.1.1 a buffered open
resumed at the raw file length, so records appended after a torn
tail were unreadable.

For unrecoverable states (`BadMagic`, `LengthOverflow`), the
reader does **not** auto-truncate: these indicate format-level
corruption that may extend beyond the tail. Opening such a
journal fails with `InvalidData` in both modes (buffered opens
accepted it before 1.1.1). Surface to a human operator for
triage.

#### Zero runs

A frame never starts with a zero byte, so the reader classifies a
zero run at a frame position by where it ends: at end of file it
is a clean end; at a 512-byte boundary within 64 MiB it is a gap a
1.1.0 Direct-IO writer left when it rotated a log-buffer slot or
resumed on a sector boundary, and the reader skips it; anywhere
else it is a reservation that was never written and iteration
stops with `TruncatedHeader`. 1.1.1 writers never leave gaps; the
skip keeps 1.1.0 journals readable. Fixtures written by 1.1.0
(`tests/fixtures/journal_v1_1_0/`) pin this.

### Durable frontier

`sync_through` publishes a durable frontier (`synced_lsn`) only
over bytes that reached the file before the fsync. Buffered
appenders register with a write gate before they reserve their
LSN range, and the group-commit leader waits for every write
reserved below its frontier to finish before it fsyncs. Direct-IO
leaders publish the end LSN captured under the log-buffer lock
when they flushed. Before 1.1.1 the frontier could cover a slow
appender's range that was still in its buffer, and that
appender's own `sync_through` then returned without any fsync
covering its bytes.

### Write and sync failures

The first failed write, flush or fsync poisons the journal. Every
later `append` / `append_batch`, and every `sync_through` whose
target is not already durable, returns an error that names the
first failure. The journal does not retry a failed fsync: Linux
may drop the dirty pages behind a failed fsync and report success
for a retry ("fsyncgate"). Recovery is to drop the handle and
reopen; the open stops at the first hole or torn frame. Before
1.1.1 only the caller whose call failed saw the error, later
syncs could succeed past a hole, and a failed Direct-IO flush
inside `sync_through` deadlocked every later `sync_through`.

### Direct-IO journal mode (0.9.5+ dual-buffer)

With `JournalOptions::direct(true)`, the journal opens the file
with `O_DIRECT` (Linux) / `F_NOCACHE` (macOS) /
`FILE_FLAG_NO_BUFFERING` (Windows). Appends route through a
dual-buffered sector-aligned log buffer; the buffer flushes to
disk at sector boundaries.

The same crash-safety contract applies, with one additional
recovery detail: the resume path scans the existing file
forward, finds the LSN past the last cleanly-decoded frame, and
**re-seats the log buffer at the largest sector boundary at or
before that LSN**. Bytes past that sector are truncated, and the
partial trailing sector is read back (through a buffered handle)
into the buffer's first sector so subsequent flushes overwrite
the existing on-disk zero-pad without destroying records. The
sector size is probed after the file is created, so a new
journal and its reopens use the same size.

Records stay contiguous across log-buffer slot rotations and
oversize records: a rotation writes the slot up to its last
partial sector and carries that sector into the next slot.
Before 1.1.1 a rotation skipped to the next slot boundary and
left a zero gap, and an oversize record raced concurrent appends
for the same LSNs.

### Test harness — `tests/crash_journal.rs`

The journal has its own subprocess-kill harness independent of
the per-method crash tests above. The harness:

1. Spawns a victim subprocess that opens a journal, appends
   `SYNCED_COUNT = 50` records, calls `sync_through` to make
   those records durable, signals `BEGIN`, then continues
   appending more records **without syncing**.
2. The parent kills the victim mid-burst (Windows
   `TerminateProcess` / Unix `SIGKILL`).
3. The parent reopens the journal and scans it forward.

Three invariants are asserted:

- **Durability.** All 50 synced records are present, intact,
  with monotonically increasing LSNs and byte-for-byte
  matching payloads.
- **Tail truncation.** The reader stops cleanly at the first
  torn frame. Tail state is one of `CleanEnd`,
  `TruncatedHeader`, `TruncatedPayload`, or `ChecksumMismatch`
  — never `BadMagic` or `LengthOverflow`.
- **No torn-frame surface.** Records past the sync barrier may
  or may not be present, but any record the reader does surface
  matches its expected payload byte-for-byte (CRC-32C enforces
  this).

Both `JournalOptions::default()` (buffered/lock-free) and
`JournalOptions::direct(true)` (Direct-IO log buffer) pass the
harness across 10 consecutive runs.

1.1.1 adds, in both modes:

- A concurrent-appender victim: four threads append and
  `sync_through` until the parent kills the process. Every
  acknowledged sync must survive, no record may be torn or
  duplicated, and the journal must reopen and accept appends.
- An injected write failure (Linux): the victim caps its file
  size with `RLIMIT_FSIZE`, so a write fails with `EFBIG`. The
  journal must stay poisoned, keep every record synced before
  the failure, and reopen cleanly.

`tests/journal_integrity.rs` covers the remaining failure modes
with file manipulation (torn and zero tails, unwritten holes,
stale frames past the resume point) and multi-threaded stress
(oversize appends racing small ones, the durable frontier under
a slow appender, preallocation racing appends).

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
- `Handle::punch_hole(path, offset, len)` (0.9.5) — deallocates
  the range, leaving a sparse hole. Crash mid-call leaves either
  the original data or the hole at that range; the syscall is
  atomic at the kernel level (Linux `fallocate`, macOS
  `fcntl(F_PUNCHHOLE)`, Windows `FSCTL_SET_ZERO_DATA`).
- `Handle::write_zeros(path, offset, len)` (0.9.5) — same
  atomicity guarantees as `punch_hole`; the range either reads
  as zeros after the call or retains its original content.
- `Handle::copy(src, dst)` — on APFS / ReFS (0.9.6 reflink
  fast-path), the clone syscall is atomic at the kernel level.
  Falls back to `std::fs::copy` on unsupported filesystems
  (which is not atomic — partial copies are possible on crash).
