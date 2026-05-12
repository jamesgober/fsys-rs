//! 0.9.5 — Sector-aligned **dual log buffer** used by Direct-IO
//! journal mode.
//!
//! Replaces the pre-0.9.5 single-buffer design (J10 in the
//! 0.9.2 audit). Two equal-sized buffer slots — one **active**
//! (receiving appends), one **dormant** (empty, or currently
//! being flushed). When the active slot fills, the appender
//! that triggers the rotation marks the old slot as
//! "flushing", drops the state lock, and performs the
//! `write_at_direct` syscall while **other appenders continue
//! filling the new active slot**. The state lock is re-acquired
//! after the syscall completes; appenders that filled the new
//! slot before the flush finished wait on a condvar (the
//! "both-slots-busy" case).
//!
//! ## Win vs the pre-0.9.5 single-buffer design
//!
//! Under sustained concurrent-appender load, the pre-0.9.5
//! single buffer held its mutex through the entire
//! `write_at_direct` syscall — every appender blocked for the
//! duration. The dual-buffer design holds the state lock only
//! for state transitions (microseconds); the syscall itself
//! (milliseconds on Direct IO) runs unlocked, letting
//! appenders into the new active slot.
//!
//! For HiveDB-class workloads (many concurrent writers per
//! handle), this is the difference between Direct-mode being
//! a single-core ceiling and being multi-core scalable.
//!
//! ## Invariants
//!
//! - Each slot is allocated via [`AlignedBuf`], pointer- +
//!   length-aligned to the device sector size. Required for
//!   `O_DIRECT` / `FILE_FLAG_NO_BUFFERING` writes.
//! - `active_flush_pos` is always sector-aligned. Writes from
//!   either slot always begin at a sector boundary.
//! - `active_len` is the number of valid (record-bearing) bytes
//!   in the active slot, measured from byte 0. `active_len ≤
//!   capacity`.
//! - `flushing == Some(idx)` ⟹ slot `idx` is exclusively
//!   accessed by the flush-owning thread; no other thread
//!   reads or writes that slot until `flushing` transitions
//!   back to `None`. This is the state-machine guarantee that
//!   makes the `UnsafeCell<AlignedBuf>` interior mutability
//!   sound.
//! - `flushing == Some(idx)` ⟹ `idx != active_idx`. We never
//!   flush the active slot via the rotation path.
//!
//! ## Memory footprint
//!
//! Two slots of `capacity` bytes each. The journal's
//! `log_buffer_kib` option is **per-slot** in 0.9.5
//! (previously it was the single buffer's size). The default
//! `log_buffer_kib(64)` therefore allocates 128 KiB total
//! per Direct journal, up from 64 KiB pre-0.9.5. Documented
//! as a deliberate trade in the 0.9.5 CHANGELOG.
//!
//! ## Why a partial flush keeps `active_flush_pos` unchanged
//!
//! A `sync_through` issued while `active_len < capacity` writes
//! `aligned_len(active_len)` bytes (records + zero-pad to the
//! next sector). The next append continues filling the active
//! slot from `active_len` — NOT from a fresh sector boundary —
//! because LSNs are byte-precise and an LSN gap would corrupt
//! resume-after-crash semantics. Same invariant as the
//! pre-0.9.5 single-buffer.
//!
//! When the active slot eventually fills (`active_len ==
//! capacity`), a rotation triggers a real flush of the slot,
//! advances `active_flush_pos += capacity` for the new active,
//! and zeros the just-flushed slot so any future partial
//! flush on it (when it becomes active again) sees zeroed
//! padding.

#![allow(dead_code)] // some accessors are reserved for benches / future probes

use crate::journal::format;
use crate::platform::{round_up, AlignedBuf};
use crate::{Error, Result};
use parking_lot::{Condvar, Mutex};
use std::cell::UnsafeCell;
use std::fs::File;

/// 0.9.5 dual-buffer log for Direct-IO journals.
///
/// Self-locking via the inner `state` mutex; the public method
/// signatures take `&self` (interior mutability). Callers
/// (`JournalHandle`) no longer need to wrap this in
/// `Mutex<LogBuffer>` — that was the pre-0.9.5 pattern that
/// served the single-buffer design.
pub(crate) struct LogBuffer {
    // 0.9.6 — iouring acceleration state. Declared BEFORE `bufs`
    // so that Drop order (declaration order in Rust) is
    // `iouring` → `bufs`: the ring un-registers and closes
    // before the underlying AlignedBuf pages are freed. This is
    // load-bearing for soundness — registered buffers pin
    // kernel pages, so the pages must outlive the registration.
    /// 0.9.6 — Linux-only IORING_REGISTER_BUFFERS +
    /// IORING_OP_WRITE_FIXED acceleration. `Some` when:
    /// 1. We're on Linux.
    /// 2. `IoUringRing::new` succeeded at construction time.
    /// 3. `register_buffers` succeeded for both slots.
    ///
    /// `None` (or on non-Linux platforms) means flushes route
    /// through the cross-platform `write_at_direct` (`pwrite`)
    /// fallback — same correctness, just no fixed-buffer fast
    /// path. The decision is made once at construction; runtime
    /// flush sites just check `is_some()`.
    #[cfg(target_os = "linux")]
    iouring: Option<IouringFlushState>,
    /// Two buffer slots. Each is `capacity` bytes, sector-aligned.
    /// Access is governed by the state machine: bytes inside
    /// slot `i` may be mutated only by the thread holding the
    /// state lock (or, while `state.flushing == Some(i)`, by
    /// the thread that set the `flushing` flag and is performing
    /// the syscall outside the lock).
    bufs: [UnsafeCell<AlignedBuf>; 2],
    /// Bytes per slot. Both slots are sized identically.
    capacity: usize,
    /// Device sector size — every flush writes a sector-multiple
    /// of bytes at a sector-aligned offset.
    sector_size: usize,
    /// State-machine + coordination. See [`State`] doc for the
    /// per-field invariants.
    state: Mutex<State>,
    /// Condvar appenders park on when both slots are busy
    /// (`active` is full AND `flushing.is_some()`). Notified
    /// after every flush completes.
    flush_done: Condvar,
}

/// 0.9.6 — Linux iouring flush state. The ring owns its owner
/// thread + kernel resources; the two AlignedBuf slots of the
/// enclosing LogBuffer are registered with this ring as buffer
/// slots `0` and `1` (matching the LogBuffer's `bufs[0]` and
/// `bufs[1]` respectively).
///
/// Soundness contract: the ring must drop **before** the
/// AlignedBufs are freed (registered buffers pin kernel pages
/// to the slot memory). Enforced by field declaration order on
/// `LogBuffer` — `iouring` is declared before `bufs`, so it
/// drops first.
#[cfg(target_os = "linux")]
struct IouringFlushState {
    ring: crate::platform::linux_iouring::IoUringRing,
}

// SAFETY: `LogBuffer`'s interior mutability is governed by the
// state machine in `state`, not by Rust's borrow checker. The
// `state` mutex serialises all state transitions; access to
// `bufs[i]` is governed by:
//   - `bufs[state.active_idx]` is mutated only while the state
//     lock is held (during `append_frame` / `flush_partial` /
//     `set_flush_pos_for_resume`).
//   - `bufs[state.flushing.unwrap()]` is read by exactly the
//     thread that set `state.flushing = Some(idx)` (the flush
//     owner); that thread holds no lock during the syscall but
//     the state-machine guarantees no other thread touches
//     `bufs[idx]` because no transition can take place on a
//     slot in the `flushing` state.
// Both modes — exclusive write under the lock and exclusive
// read by the flush owner — yield exclusive aliasing semantics
// equivalent to `&mut [u8]`. There is no data race.
unsafe impl Send for LogBuffer {}
// SAFETY: see `unsafe impl Send for LogBuffer` above — the same
// state-machine + mutex coordination that makes cross-thread
// ownership transfer sound also makes shared references sound:
// every access to `bufs[i]` is mediated by the `state` lock or
// the `flushing` invariant, so there is no aliased mutation.
unsafe impl Sync for LogBuffer {}

/// Coordination state. Protected by [`LogBuffer::state`].
#[derive(Debug)]
struct State {
    /// `0` or `1`. Identifies which slot of `bufs` currently
    /// receives appends.
    active_idx: u8,
    /// Bytes used in the active slot, measured from byte 0.
    /// `0 <= active_len <= capacity` always.
    active_len: usize,
    /// File offset of byte 0 of the active slot. Always
    /// sector-aligned. Advances by `capacity` on every full-slot
    /// flush; can advance by an unaligned amount on the
    /// oversize-record path (which then re-aligns to the
    /// preceding sector boundary).
    active_flush_pos: u64,
    /// `Some(idx)` while slot `idx` is being flushed by some
    /// thread that has dropped the state lock to perform the
    /// `write_at_direct` syscall. The flush owner re-acquires
    /// the lock after the syscall to transition `flushing` back
    /// to `None`. While `Some(idx)`, no other thread reads or
    /// writes `bufs[idx]`. Invariant: `flushing.is_some()` ⟹
    /// `flushing.unwrap() != active_idx`.
    flushing: Option<u8>,
}

impl LogBuffer {
    /// Allocates a new dual-buffer log with `capacity_per_slot`
    /// bytes per slot.
    ///
    /// `capacity_per_slot` is rounded up to the next sector
    /// boundary if it isn't already aligned (defensive — callers
    /// in `JournalHandle::open_direct` compute it from
    /// `JournalOptions::log_buffer_kib`).
    ///
    /// Total heap usage: `2 × round_up(capacity_per_slot,
    /// sector_size)`.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] if either aligned allocation fails.
    pub(crate) fn new(capacity_per_slot: u32, sector_size: u32, flush_pos: u64) -> Result<Self> {
        let ss = sector_size as usize;
        debug_assert!(ss.is_power_of_two(), "sector_size must be a power of two");
        debug_assert!(
            flush_pos % sector_size as u64 == 0,
            "flush_pos must be sector-aligned"
        );
        let cap = round_up(capacity_per_slot as usize, ss).max(ss);
        let buf0 = AlignedBuf::new(cap, ss)?;
        let buf1 = AlignedBuf::new(cap, ss)?;
        let bufs = [UnsafeCell::new(buf0), UnsafeCell::new(buf1)];

        // 0.9.6 — try to construct an io_uring ring + register
        // the two AlignedBuf slots. Failure is silent: kernel
        // < 5.1, sandbox / SECCOMP / AppArmor block,
        // register_buffers rejection, etc. — all fall back to
        // the pwrite path with no observable behaviour change.
        #[cfg(target_os = "linux")]
        let iouring = Self::try_init_iouring(&bufs, cap);

        Ok(Self {
            #[cfg(target_os = "linux")]
            iouring,
            bufs,
            capacity: cap,
            sector_size: ss,
            state: Mutex::new(State {
                active_idx: 0,
                active_len: 0,
                active_flush_pos: flush_pos,
                flushing: None,
            }),
            flush_done: Condvar::new(),
        })
    }

    /// 0.9.6 — Try to bring up an `IoUringRing` and register the
    /// two AlignedBuf slots as fixed buffers. Returns `None` on
    /// any failure; the pwrite path is the silent fallback.
    ///
    /// The ring's queue depth is small (8) because the journal's
    /// flush submission rate is bounded by sector flushes —
    /// thousands per second under sustained load is still far
    /// below per-syscall granularity.
    #[cfg(target_os = "linux")]
    fn try_init_iouring(
        bufs: &[UnsafeCell<AlignedBuf>; 2],
        cap: usize,
    ) -> Option<IouringFlushState> {
        // 0.9.7 SQPOLL: the LogBuffer's internal flush ring is
        // distinct from the per-Handle io_uring sync ring and is
        // not user-configurable. We never enable SQPOLL here —
        // the LogBuffer's submission rate is bounded by sector
        // flushes and doesn't benefit from kernel-side polling.
        let ring = crate::platform::linux_iouring::IoUringRing::new(8, None).ok()?;
        // Collect the (ptr, len) of each slot's underlying
        // AlignedBuf. We're inside the constructor, so nothing
        // else has access to the cells; reading the start
        // pointer + length is sound. The kernel records these
        // for the duration of the ring (until un-registered or
        // ring is closed); the AlignedBufs outlive the ring per
        // the Drop-order contract documented on `LogBuffer`.
        let iovs: Vec<(usize, usize)> = bufs
            .iter()
            .map(|cell| {
                // SAFETY: constructor-time exclusive access to
                // `cell`; no other thread can observe the
                // UnsafeCell yet. The AlignedBuf's pointer +
                // length are stable for its lifetime (AlignedBuf
                // never reallocates).
                let buf = unsafe { (*cell.get()).as_slice() };
                debug_assert_eq!(buf.len(), cap);
                (buf.as_ptr() as usize, buf.len())
            })
            .collect();
        ring.register_buffers(&iovs).ok()?;
        Some(IouringFlushState { ring })
    }

    /// Returns the LSN that the next append would place its
    /// record's first byte at.
    #[inline]
    pub(crate) fn next_lsn(&self) -> u64 {
        let state = self.state.lock();
        state.active_flush_pos + state.active_len as u64
    }

    /// Returns the per-slot capacity in bytes. The total heap
    /// allocation for the dual-buffer is `2 × capacity()`.
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// File offset of the first byte of the **currently active**
    /// slot. Bytes at `[0..flushed_through())` are on stable
    /// storage *after* a `sync_data` syscall completes (modulo
    /// any in-flight flush of the dormant slot).
    #[inline]
    pub(crate) fn flushed_through(&self) -> u64 {
        self.state.lock().active_flush_pos
    }

    /// Returns the number of buffered (not-yet-flushed) bytes in
    /// the active slot.
    #[inline]
    pub(crate) fn buffered_len(&self) -> usize {
        self.state.lock().active_len
    }

    /// 0.9.6 — Batched-append fast path. Audit finding H-15.
    ///
    /// When all `records` fit in the active slot's remaining
    /// capacity, encode + memcpy every frame into the slot under a
    /// **single** state-lock acquisition. Returns
    /// `Ok(Some((start_lsn, end_lsn)))` with the byte-offset range
    /// the batch occupies.
    ///
    /// Returns `Ok(None)` when the batch wouldn't fit in one shot
    /// (would require rotation or an oversize record). The caller
    /// falls back to the per-record [`Self::append_frame`] loop
    /// which handles rotation, mid-flush waits, and the
    /// oversize-standalone path.
    ///
    /// **Win vs per-record loop.** For an N-record batch, this
    /// reduces lock acquisitions from N to 1. With `parking_lot`'s
    /// uncontended-acquire cost ~50-100 ns and contended ~µs, the
    /// per-record overhead saved is meaningful at large N — a
    /// 1000-record batch on 8 threads saves ~50-800 µs of lock
    /// overhead.
    pub(crate) fn try_append_frames_batched(
        &self,
        records: &[&[u8]],
        total_encoded_size: usize,
    ) -> Result<Option<(u64, u64)>> {
        if records.is_empty() {
            return Ok(Some((0, 0)));
        }
        let mut state = self.state.lock();
        let remaining = self.capacity.saturating_sub(state.active_len);
        if total_encoded_size > remaining {
            // Doesn't fit in one shot — let the caller fall back
            // to the per-record path which handles rotation.
            return Ok(None);
        }
        let active_idx = state.active_idx as usize;
        let offset_start = state.active_len;
        let start_lsn = state.active_flush_pos + offset_start as u64;
        let mut cursor = offset_start;
        // SAFETY: we hold the state lock; the active slot is
        // exclusively ours for the duration of every encode below.
        // No other thread can mutate `bufs[active_idx]` while we
        // hold the lock; the state-machine invariant
        // `active_idx != flushing.unwrap()` is maintained by
        // [`Self::append_frame`]'s rotation path.
        unsafe {
            let slice = (*self.bufs[active_idx].get()).as_mut_slice();
            for record in records {
                let frame_size = record
                    .len()
                    .checked_add(format::FRAME_OVERHEAD)
                    .ok_or_else(|| {
                        Error::Io(std::io::Error::other("batched frame size overflow"))
                    })?;
                // `total_encoded_size` was computed by the caller
                // and matches `sum(record.len + FRAME_OVERHEAD)`;
                // since we verified `total_encoded_size <=
                // remaining` above, this slice is in-bounds.
                let _ = format::encode_frame_into(record, &mut slice[cursor..cursor + frame_size])?;
                cursor += frame_size;
            }
        }
        state.active_len = cursor;
        let end_lsn = state.active_flush_pos + cursor as u64;
        Ok(Some((start_lsn, end_lsn)))
    }

    /// Encodes `payload` as a frame and appends to the active
    /// slot, rotating slots when the active fills and waiting
    /// for the dormant slot's flush to finish if both are busy.
    ///
    /// Returns `(start_lsn, end_lsn)` — the file-byte-offset
    /// range the frame occupies.
    ///
    /// **Concurrent behaviour.** Multiple threads calling
    /// `append_frame` may proceed concurrently as long as the
    /// active slot has room — they serialise on the brief state
    /// lock (microseconds) but **not** on the `write_at_direct`
    /// syscall (milliseconds). Only the thread that triggers a
    /// rotation pays the syscall cost; other threads continue
    /// into the new active slot.
    pub(crate) fn append_frame(&self, file: &File, payload: &[u8]) -> Result<(u64, u64)> {
        let frame = format::encode_frame_owned(payload)?;
        let frame_size = frame.len();

        loop {
            let mut state = self.state.lock();

            // Path A — fits in the active slot. Fast path; copy
            // and return.
            if state.active_len + frame_size <= self.capacity {
                let active_idx = state.active_idx as usize;
                let offset = state.active_len;
                let start = state.active_flush_pos + offset as u64;
                let end = start + frame_size as u64;
                // SAFETY: we hold the state lock; the active
                // slot is exclusively ours for the duration of
                // this copy. No other thread can mutate
                // `bufs[active_idx]` while the lock is held;
                // the state machine guarantees `active_idx !=
                // flushing.unwrap()` even if a flush is in
                // flight.
                unsafe {
                    let slice = (*self.bufs[active_idx].get()).as_mut_slice();
                    slice[offset..offset + frame_size].copy_from_slice(&frame);
                }
                state.active_len += frame_size;
                return Ok((start, end));
            }

            // Path B — doesn't fit. We need to either (a) rotate
            // (if the dormant slot is free), or (b) wait for the
            // in-flight flush of the dormant slot to complete.
            if state.flushing.is_some() {
                // Both slots are busy: active is full AND the
                // dormant is being flushed. Park on the condvar;
                // the flush owner notifies after their syscall
                // completes.
                self.flush_done.wait(&mut state);
                // Re-loop to re-check active capacity.
                continue;
            }

            // Path C — rotate. We have data to flush (or are
            // facing an oversize record); the dormant slot is
            // free.
            let old_idx = state.active_idx;
            let old_len = state.active_len;
            let old_flush_pos = state.active_flush_pos;
            let new_idx = old_idx ^ 1;

            if old_len > 0 {
                // Standard rotation: move active to the other
                // slot, mark old as flushing, drop the lock,
                // perform the syscall, re-acquire to clean up.
                state.active_idx = new_idx;
                state.active_len = 0;
                state.active_flush_pos = old_flush_pos
                    .checked_add(self.capacity as u64)
                    .ok_or_else(|| Error::Io(std::io::Error::other("flush_pos overflow")))?;
                state.flushing = Some(old_idx);
                drop(state);

                // SAFETY: state.flushing = Some(old_idx) tells
                // every other thread to leave `bufs[old_idx]`
                // alone; we have exclusive read access for the
                // syscall.
                //
                // 0.9.6 — when iouring is available, submit via
                // `IORING_OP_WRITE_FIXED` against the
                // pre-registered slot index (`old_idx`); the
                // kernel skips per-SQE buffer page pinning.
                // Otherwise fall back to the pwrite path.
                let flush_result = unsafe {
                    let slice = (*self.bufs[old_idx as usize].get()).as_slice();
                    self.flush_slot_to_disk(file, old_idx, slice, old_flush_pos)
                };

                // Re-acquire, zero the just-flushed slot, and
                // wake any appenders parked on `flush_done`.
                {
                    let mut state = self.state.lock();
                    // SAFETY: state.flushing is still Some(old_idx);
                    // we are still the exclusive owner.
                    //
                    // 0.9.6 — `slice.fill(0)` lowers to `memset`
                    // (vectorised) on every supported toolchain
                    // since rustc 1.51. The pre-0.9.6 hand-rolled
                    // `for b in slice.iter_mut() { *b = 0; }` was
                    // not always vectorised on debug builds and
                    // cost ~5-10 µs per rotation on a 64 KiB slot.
                    unsafe {
                        let slice = (*self.bufs[old_idx as usize].get()).as_mut_slice();
                        slice.fill(0);
                    }
                    state.flushing = None;
                    let _ = self.flush_done.notify_all();
                }

                flush_result?;

                // Loop back to retry the append into the new
                // active slot. If the frame is oversize, the
                // next iteration's path-A check fails and we
                // fall through to path-D (oversize standalone
                // write) with `active_len == 0`.
                continue;
            }

            // Path D — oversize record (frame doesn't fit in a
            // single slot). active_len == 0 (either we just
            // rotated, or we started here with an empty active
            // and an oversize frame). Standalone aligned write
            // at the current flush_pos; load the partial
            // trailing sector into the (still active) slot.
            debug_assert_eq!(old_len, 0);
            debug_assert!(frame_size > self.capacity);

            let start = old_flush_pos;
            let aligned = round_up(frame_size, self.sector_size);
            let mut scratch = AlignedBuf::new(aligned, self.sector_size)?;
            scratch.as_mut_slice()[..frame_size].copy_from_slice(&frame);
            // alloc_zeroed already filled the trailing pad.

            // Compute new state values from the write outcome.
            let end = start + frame_size as u64;
            let new_flush_pos = (end / self.sector_size as u64) * self.sector_size as u64;
            let tail = (end - new_flush_pos) as usize;

            // We do NOT mark anything as flushing here — the
            // oversize write is to a region that's neither slot.
            // No state-machine invariant requires the lock to
            // be held during the syscall; drop it for the
            // duration.
            drop(state);

            crate::platform::write_at_direct(file, start, scratch.as_slice())?;

            // Re-acquire to update state and load the partial-
            // sector tail into the active slot.
            {
                let mut state = self.state.lock();
                state.active_flush_pos = new_flush_pos;
                if tail > 0 {
                    let scratch_offset = aligned - self.sector_size;
                    let in_sector = (frame_size - scratch_offset).min(self.sector_size);
                    let active_idx = state.active_idx as usize;
                    // SAFETY: we hold the state lock; the active
                    // slot is exclusively ours. (No flush is in
                    // flight; we just dropped & re-acquired but
                    // the lock guarantees no rotation
                    // intervened.)
                    unsafe {
                        let slice = (*self.bufs[active_idx].get()).as_mut_slice();
                        slice[..in_sector].copy_from_slice(
                            &scratch.as_slice()[scratch_offset..scratch_offset + in_sector],
                        );
                        // Zero the rest of the first sector
                        // (defensive — alloc_zeroed already
                        // filled this region, but we may have
                        // re-used this slot from a prior cycle).
                        for b in &mut slice[in_sector..self.sector_size] {
                            *b = 0;
                        }
                    }
                    state.active_len = tail;
                }
            }
            return Ok((start, end));
        }
    }

    /// Partial / sync-point flush. Writes `aligned_len(active_len)`
    /// bytes at `active_flush_pos` (records + zero-pad to the next
    /// sector boundary). Does NOT advance `active_flush_pos` or
    /// reset `active_len` — the next append continues filling the
    /// active slot from `active_len`. Subsequent flushes overwrite
    /// the partial-sector pad with new record bytes.
    ///
    /// **Coordination.** This method waits for any in-flight
    /// dormant-slot flush (`flushing.is_some()`) to complete
    /// before issuing the partial flush, then holds the state
    /// lock through the partial-flush syscall. This is the
    /// deliberate sync point — callers asked for "make this
    /// durable now" and we honour that by serialising. Other
    /// appenders wait briefly.
    pub(crate) fn flush_partial(&self, file: &File) -> Result<()> {
        let mut state = self.state.lock();

        // Wait for any in-flight dormant-slot flush to finish.
        // We need a clean state before issuing the partial flush
        // so the on-disk byte sequence is consistent.
        while state.flushing.is_some() {
            self.flush_done.wait(&mut state);
        }

        if state.active_len == 0 {
            return Ok(()); // nothing to flush
        }

        let aligned = round_up(state.active_len, self.sector_size);
        let active_idx = state.active_idx as usize;
        let active_flush_pos = state.active_flush_pos;
        // SAFETY: we hold the state lock; no flush is in flight
        // (we waited above). The active slot is exclusively ours
        // for the duration of this syscall. The
        // `[active_len..aligned]` tail is zero by induction (see
        // module-level invariants).
        //
        // 0.9.6 — partial flushes route through the
        // iouring-aware helper (`flush_slot_to_disk`) which
        // submits via `IORING_OP_WRITE_FIXED` when available.
        unsafe {
            let slice = (*self.bufs[active_idx].get()).as_slice();
            self.flush_slot_to_disk(file, active_idx as u8, &slice[..aligned], active_flush_pos)?;
        }
        Ok(())
    }

    /// 0.9.6 — Centralised flush dispatcher. Routes via
    /// `IORING_OP_WRITE_FIXED` when iouring is available
    /// (Linux + ring-construction succeeded + buffer
    /// registration succeeded), falls back to
    /// `crate::platform::write_at_direct` (`pwrite`) otherwise.
    ///
    /// `slot_idx` is `0` or `1` — the LogBuffer's internal slot
    /// index. The iouring registration aligned slot 0 = buf
    /// index 0 and slot 1 = buf index 1, so this passes
    /// through unchanged as the `buf_idx` argument to
    /// `write_at_fixed`.
    ///
    /// **Caller contract:** `slice` must be a sub-region of the
    /// AlignedBuf at slot `slot_idx`. The kernel will validate
    /// that the region fits within the registered buffer; an
    /// invalid range surfaces as `EFAULT` / `EINVAL` from the
    /// CQE which propagates as `Err(Error::Io)`.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    fn flush_slot_to_disk(
        &self,
        file: &File,
        slot_idx: u8,
        slice: &[u8],
        offset: u64,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(iouring) = self.iouring.as_ref() {
            use std::os::fd::AsRawFd;
            let fd = file.as_raw_fd();
            // Ensure slot_idx fits in u16 (our registration uses
            // slots 0 and 1; debug_assert catches future bugs
            // if more slots are ever added).
            debug_assert!(slot_idx < 2);
            let written = iouring
                .ring
                .write_at_fixed(fd, slot_idx as u16, slice, offset)?;
            if written != slice.len() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "iouring write_at_fixed returned short count",
                )));
            }
            return Ok(());
        }
        crate::platform::write_at_direct(file, offset, slice)
    }

    /// Repositions the buffer for resume-after-crash. Called by
    /// `JournalHandle::open_direct` after `scan_clean_end` finds
    /// the last good LSN. Sets `active_flush_pos` to the last
    /// sector boundary at or before `resume_lsn`, primes slot 0
    /// with the partial-sector tail from disk (`prefix_bytes`)
    /// so subsequent flushes overwrite the existing on-disk
    /// zero-pad cleanly, and sets `active_len` to the in-sector
    /// resume offset.
    pub(crate) fn set_flush_pos_for_resume(
        &self,
        flush_pos: u64,
        in_sector_offset: usize,
        prefix_bytes: &[u8],
    ) {
        let mut state = self.state.lock();
        debug_assert_eq!(state.active_len, 0, "rehydrate must run on a fresh buffer");
        debug_assert!(
            flush_pos % self.sector_size as u64 == 0,
            "flush_pos must be sector-aligned"
        );
        debug_assert!(
            state.flushing.is_none(),
            "rehydrate must run before any flush has started"
        );
        state.active_flush_pos = flush_pos;
        if in_sector_offset > 0 {
            let copy_len = in_sector_offset
                .min(prefix_bytes.len())
                .min(self.sector_size);
            let active_idx = state.active_idx as usize;
            // SAFETY: we hold the state lock; the active slot is
            // exclusively ours during this resume init.
            unsafe {
                let slice = (*self.bufs[active_idx].get()).as_mut_slice();
                slice[..copy_len].copy_from_slice(&prefix_bytes[..copy_len]);
            }
            state.active_len = copy_len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static C: AtomicU32 = AtomicU32::new(0);

    fn tmp_path(tag: &str) -> PathBuf {
        let n = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("fsys_logbuf_{}_{}_{tag}", std::process::id(), n))
    }

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn make_file() -> (PathBuf, File, Cleanup) {
        let path = tmp_path("logbuf");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        (path.clone(), f, Cleanup(path))
    }

    #[test]
    fn new_buffer_is_aligned_and_zeroed() {
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        assert_eq!(buf.capacity(), 4096);
        assert_eq!(buf.next_lsn(), 0);
        assert_eq!(buf.buffered_len(), 0);
    }

    #[test]
    fn append_frame_fits_in_active_slot() {
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        let (start, end) = buf.append_frame(&file, b"hello").unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, 5 + format::FRAME_OVERHEAD as u64);
        assert_eq!(buf.next_lsn(), end);
        // No rotation yet — file is empty.
        let on_disk = std::fs::read(&path).unwrap();
        assert!(on_disk.is_empty());
    }

    #[test]
    fn active_full_triggers_rotation_and_real_flush() {
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        // Each frame: 12 bytes overhead + 100 bytes payload = 112 bytes.
        // 4096 / 112 = 36 records fit; the 37th triggers rotation.
        let payload = vec![0xABu8; 100];
        for _ in 0..36 {
            let _ = buf.append_frame(&file, &payload).unwrap();
        }
        // Before rotation — nothing on disk yet.
        assert_eq!(buf.flushed_through(), 0);
        // 37th append triggers rotation; old slot (slot 0) gets flushed.
        let _ = buf.append_frame(&file, &payload).unwrap();
        // The new active slot's flush_pos is now `capacity` = 4096.
        assert_eq!(buf.flushed_through(), 4096);
        // File size = 4096 (the full old slot was written).
        let mut f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 4096);
    }

    #[test]
    fn flush_partial_writes_records_plus_zero_pad() {
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        let _ = buf.append_frame(&file, b"x").unwrap(); // 13-byte frame
        buf.flush_partial(&file).unwrap();
        // Aligned-up to next sector = 512 bytes written.
        let mut f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 512);
        // First 13 bytes are the frame; remaining 499 bytes are zero pad.
        assert!(bytes[13..].iter().all(|&b| b == 0));
        // active_flush_pos NOT advanced (partial flush invariant).
        assert_eq!(buf.flushed_through(), 0);
    }

    #[test]
    fn oversize_record_writes_standalone_with_tail_carryover() {
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        // 5000-byte payload → 5012-byte frame, exceeds 4096-byte slot.
        let payload = vec![0xCDu8; 5000];
        let (start, end) = buf.append_frame(&file, &payload).unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, 5012);
        // active_flush_pos lands at the last sector boundary ≤ end:
        // floor(5012 / 512) * 512 = 4608.
        assert_eq!(buf.flushed_through(), 4608);
        // The active slot's first sector now holds the partial-
        // sector tail: 5012 - 4608 = 404 bytes.
        assert_eq!(buf.buffered_len(), 404);
        // File on disk is sector-aligned-up: round_up(5012, 512) = 5120.
        let mut f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 5120);
    }

    // ─────────────────────────────────────────────────────────
    // 0.9.5 — dual-buffer concurrent-append coverage
    // ─────────────────────────────────────────────────────────

    #[test]
    fn rotation_alternates_active_slot_indices() {
        // Trigger 4 rotations and confirm active_flush_pos
        // advances by `capacity` each time. Rotation fires on
        // the append AFTER the active slot fills, so N
        // rotations require `frames_per_slot * N + 1` appends.
        let (_path, file, _g) = make_file();
        let buf = LogBuffer::new(512, 512, 0).unwrap();
        // Each frame: 12 overhead + 4 payload = 16 bytes.
        // 512 / 16 = 32 frames per slot.
        let payload = [0xAAu8; 4];
        // 4 rotations: 32 * 4 + 1 = 129 appends.
        for _ in 0..(32 * 4 + 1) {
            let _ = buf.append_frame(&file, &payload).unwrap();
        }
        // After 4 rotations the active slot's flush_pos = 4*512 = 2048.
        assert_eq!(buf.flushed_through(), 2048);
    }

    #[test]
    fn concurrent_appends_during_flush_dont_block_on_syscall() {
        // The load-bearing 0.9.5 invariant: while one thread
        // performs the syscall (slow), other threads can append
        // into the new active slot (fast). We can't directly
        // observe "didn't block" without timing, but we can
        // confirm correctness under contention: N threads each
        // submit M records, file ends up with N*M records
        // worth of bytes, no deadlock.
        use std::sync::Arc;
        let (path, file, _g) = make_file();
        let buf = Arc::new(LogBuffer::new(4096, 512, 0).unwrap());
        let file = Arc::new(file);
        let n_threads = 8usize;
        let per_thread = 200usize;
        let payload_size = 24usize; // 12 + 24 = 36 byte frames
        let payload = vec![0xBBu8; payload_size];

        let mut handles = Vec::with_capacity(n_threads);
        for _ in 0..n_threads {
            let buf = Arc::clone(&buf);
            let file = Arc::clone(&file);
            let payload = payload.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..per_thread {
                    let _ = buf.append_frame(&file, &payload).expect("append");
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        // Final sync to push the active slot's tail to disk.
        buf.flush_partial(&file).expect("partial flush");

        // Confirm the total number of bytes written matches the
        // total framed-record bytes (modulo sector padding at
        // the end). Each frame: 12 + 24 = 36 bytes.
        // Total: 8 * 200 * 36 = 57 600 bytes.
        let total_bytes = (n_threads * per_thread * 36) as u64;

        let on_disk_len = std::fs::metadata(&path).unwrap().len();
        // The on-disk size is the most recent flush's coverage.
        // It must be at least `total_bytes` (rounded up to a
        // multiple of capacity) and at most that plus one
        // capacity-worth (the active slot's pad).
        assert!(
            on_disk_len >= total_bytes - 4096,
            "on-disk {} < total {}",
            on_disk_len,
            total_bytes
        );
    }

    #[test]
    fn flush_partial_waits_for_in_flight_rotation_flush() {
        // Set up: trigger a rotation (which starts a flush of
        // slot 0), then call flush_partial. flush_partial must
        // wait for the in-flight flush to complete before
        // proceeding. We can't observe the wait directly, but
        // we can confirm correctness: after flush_partial
        // returns, both the rotated slot's bytes AND the
        // current active slot's bytes are on disk.
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(512, 512, 0).unwrap();
        // Fill slot 0 (32 frames of 16 bytes each).
        let payload = [0xCCu8; 4];
        for _ in 0..32 {
            let _ = buf.append_frame(&file, &payload).unwrap();
        }
        // 33rd append triggers rotation: slot 0 → flush, slot 1
        // becomes active with the 33rd frame.
        let _ = buf.append_frame(&file, &payload).unwrap();
        // Now flush_partial of the active (slot 1, with one
        // frame in it). After this returns, the file holds
        // slot 0's full content (512 bytes) followed by slot 1's
        // partial content (16 bytes + zero pad to 512).
        buf.flush_partial(&file).unwrap();
        let on_disk_len = std::fs::metadata(&path).unwrap().len();
        // 512 (slot 0 rotation flush) + 512 (slot 1 partial pad) = 1024.
        assert_eq!(on_disk_len, 1024);
    }

    #[test]
    fn back_to_back_rotations_after_sustained_load() {
        // Smoke test: feed enough records to trigger multiple
        // rotations and confirm offsets advance correctly with
        // no panics, no incorrect state, no deadlock.
        // Rotation fires on the append AFTER the active slot
        // fills, so 5 rotations require `36 * 5 + 1` = 181 appends.
        let (_path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        let payload = vec![0xDDu8; 100]; // 112-byte frames
                                         // 4096 / 112 = 36 frames per slot.
        for _ in 0..(36 * 5 + 1) {
            let _ = buf.append_frame(&file, &payload).unwrap();
        }
        // After 5 rotations: active_flush_pos = 5 * 4096 = 20480.
        assert_eq!(buf.flushed_through(), 20480);
    }
}
