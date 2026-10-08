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
//! - LSNs are contiguous: `active_flush_pos + active_len` is
//!   the end of the last appended record, and every byte below
//!   it belongs to a record. Rotation and the oversize path
//!   never skip to a fresh slot boundary; they carry the
//!   partial trailing sector into the active slot instead.
//! - Bytes of a slot past its `active_len` are zero (slots are
//!   zeroed when allocated and after every flush), so a flush
//!   of a partial sector writes zero padding.
//! - `flushing == Some(Slot(idx))` ⟹ slot `idx` is exclusively
//!   accessed by the flush-owning thread; no other thread
//!   reads or writes that slot until `flushing` transitions
//!   back to `None`. This is the state-machine guarantee that
//!   makes the `UnsafeCell<AlignedBuf>` interior mutability
//!   sound.
//! - `flushing == Some(Slot(idx))` ⟹ `idx != active_idx`. We
//!   never flush the active slot via the rotation path.
//! - At most one write runs outside the lock at a time
//!   (`flushing.is_some()`). Every other write (rotation,
//!   oversize record, partial flush) waits for it, which keeps
//!   writes of the shared partial sector in LSN order.
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
//! When a record does not fit in the rest of the active slot,
//! a rotation writes `round_up(active_len, sector)` bytes of
//! the slot, advances `active_flush_pos` by the whole sectors
//! it contained, and copies the partial trailing sector (the
//! bytes past the last sector boundary) into the other slot,
//! which becomes active. The record then lands right after the
//! previous one. The just-flushed slot is zeroed so any future
//! partial flush on it sees zeroed padding. When the shared
//! partial sector is written again from the new slot, that
//! write waits for the rotation flush, so the newer content
//! always lands last.
//!
//! Before 1.1.1 a rotation advanced `active_flush_pos` by the
//! full slot capacity, leaving a zero gap between the last
//! record of the old slot and the first record of the new one.
//! The reader still skips such gaps so journals written by
//! 1.1.0 stay readable.
//!
//! ## Oversize records
//!
//! A record that does not fit in a slot even after a rotation
//! (frame larger than `capacity` minus the carried tail) is
//! written straight from a private scratch buffer together with
//! the active slot's current contents. The active slot is
//! re-seated at the last sector boundary of the record and
//! holds the record's partial trailing sector. While that write
//! runs, `flushing` is `Some(Scratch)` so rotations and partial
//! flushes wait for it; appenders that fit in the active slot
//! keep going.

#![allow(dead_code)] // some accessors are reserved for benches / future probes

use crate::journal::format;
use crate::journal::poison::Poison;
use crate::platform::{round_up, AlignedBuf};
use crate::{Error, Result};
use parking_lot::{Condvar, Mutex, MutexGuard};
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
//     `set_flush_pos_for_resume`), and the dormant slot is only
//     touched under the lock while no flush is in flight (the
//     rotation copies the carried tail into it).
//   - When `state.flushing == Some(InFlight::Slot(idx))`,
//     `bufs[idx]` is read by exactly the thread that set it (the
//     flush owner); that thread holds no lock during the syscall
//     but the state-machine guarantees no other thread touches
//     `bufs[idx]` because no transition can take place on a
//     slot in the `flushing` state. `InFlight::Scratch` borrows
//     no slot.
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
    /// sector-aligned. A rotation or an oversize record advances
    /// it to the last sector boundary at or below the end of the
    /// written data.
    active_flush_pos: u64,
    /// `Some(_)` while a write runs outside the state lock. The
    /// owner re-acquires the lock after the syscall to transition
    /// `flushing` back to `None` and notify `flush_done`. See
    /// [`InFlight`] for what each variant protects.
    flushing: Option<InFlight>,
}

/// A write that runs with the state lock dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InFlight {
    /// Slot `idx` was rotated out and is being written. No other
    /// thread reads or writes `bufs[idx]` until the flush ends.
    /// Invariant: `idx != active_idx`.
    Slot(u8),
    /// An oversize record (prefixed by the active slot's earlier
    /// contents) is being written from a scratch buffer owned by
    /// the writing thread. No slot is borrowed by the write.
    Scratch,
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
    /// for an in-flight flush to finish if it must.
    ///
    /// Returns `(start_lsn, end_lsn)` — the file-byte-offset
    /// range the frame occupies. Ranges returned to concurrent
    /// callers never overlap and leave no gaps.
    ///
    /// **Concurrent behaviour.** Multiple threads calling
    /// `append_frame` may proceed concurrently as long as the
    /// active slot has room — they serialise on the brief state
    /// lock (microseconds) but **not** on the `write_at_direct`
    /// syscall (milliseconds). Only the thread that triggers a
    /// rotation or writes an oversize record pays the syscall
    /// cost; other threads continue into the active slot.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] with `InvalidInput` if `payload` exceeds
    ///   the frame format's maximum payload (checked before any
    ///   allocation).
    /// - [`Error::Io`] if a flush this call performs fails, or if
    ///   the scratch allocation for an oversize record fails. A
    ///   failed flush poisons `poison`.
    /// - The poison error if `poison` is set, including when it is
    ///   set by another thread's failed flush while this call
    ///   waits.
    pub(crate) fn append_frame(
        &self,
        file: &File,
        payload: &[u8],
        poison: &Poison,
    ) -> Result<(u64, u64)> {
        let frame_size = format::frame_len(payload.len())?;
        // Records that can fit in a slot are encoded before taking
        // the lock to keep the critical section short. Larger ones
        // are encoded straight into their scratch buffer.
        let frame = if frame_size <= self.capacity {
            Some(format::encode_frame_owned(payload)?)
        } else {
            None
        };

        loop {
            let mut state = self.state.lock();
            // A failed flush is published under this lock before
            // waiters are notified, so checking here also covers
            // threads woken from `flush_done`.
            poison.check()?;

            // Path A — fits in the active slot. Fast path; copy
            // and return.
            if let Some(frame) = frame.as_deref() {
                if state.active_len + frame_size <= self.capacity {
                    let active_idx = state.active_idx as usize;
                    let offset = state.active_len;
                    let start = state.active_flush_pos + offset as u64;
                    let end = start + frame_size as u64;
                    // SAFETY: we hold the state lock; the active
                    // slot is exclusively ours for the duration of
                    // this copy. No other thread can mutate
                    // `bufs[active_idx]` while the lock is held;
                    // the state machine guarantees an in-flight
                    // slot flush never targets the active slot.
                    unsafe {
                        let slice = (*self.bufs[active_idx].get()).as_mut_slice();
                        slice[offset..offset + frame_size].copy_from_slice(frame);
                    }
                    state.active_len += frame_size;
                    return Ok((start, end));
                }
            }

            // Path B: does not fit, and a write is in flight. Any
            // write we would issue next (rotation or oversize)
            // covers the sector that write may also cover, so it
            // must wait. The flush owner notifies on completion.
            if state.flushing.is_some() {
                self.flush_done.wait(&mut state);
                continue;
            }

            // Path C: rotate. After a rotation the active slot
            // holds only the carried partial sector; if the frame
            // fits behind it, rotate and retry. `tail + frame_size
            // <= capacity` together with the Path A miss implies
            // `active_len >= sector_size`, so every rotation
            // advances `active_flush_pos` and the loop terminates.
            let tail = state.active_len % self.sector_size;
            if tail + frame_size <= self.capacity {
                self.rotate(state, file, poison)?;
                continue;
            }

            // Path D: the frame cannot fit in a slot behind the
            // carried tail. Write it together with the active
            // slot's contents from a scratch buffer.
            return self.append_oversize(
                state,
                file,
                payload,
                frame.as_deref(),
                frame_size,
                poison,
            );
        }
    }

    /// Rotates the active slot out to disk. Caller holds the
    /// state lock (passed in as `state`) with no write in flight
    /// and a non-empty active slot.
    ///
    /// Writes `round_up(active_len, sector)` bytes of the old
    /// slot at `active_flush_pos`, advances `active_flush_pos` by
    /// the whole sectors written, and carries the trailing
    /// partial sector into the other slot, which becomes active.
    ///
    /// A failed write poisons `poison` before waiters are woken:
    /// the records in the old slot were already acknowledged and
    /// are now lost, so no later append or sync may succeed.
    fn rotate(&self, mut state: MutexGuard<'_, State>, file: &File, poison: &Poison) -> Result<()> {
        debug_assert!(state.flushing.is_none());
        let old_idx = state.active_idx;
        let old_len = state.active_len;
        let old_flush_pos = state.active_flush_pos;
        let new_idx = old_idx ^ 1;
        let whole = old_len - old_len % self.sector_size;
        let tail = old_len - whole;
        let write_len = round_up(old_len, self.sector_size);
        let new_flush_pos = old_flush_pos
            .checked_add(whole as u64)
            .ok_or_else(|| Error::Io(std::io::Error::other("flush_pos overflow")))?;

        // SAFETY: we hold the state lock and no write is in
        // flight, so both slots are exclusively ours: the old
        // (active) slot is only read here and the new slot is
        // only written. They are distinct `UnsafeCell`s, so the
        // shared and mutable borrows do not alias. The new slot
        // is all zero past `tail` by the module invariant.
        unsafe {
            let src = (*self.bufs[old_idx as usize].get()).as_slice();
            let dst = (*self.bufs[new_idx as usize].get()).as_mut_slice();
            dst[..tail].copy_from_slice(&src[whole..old_len]);
        }
        state.active_idx = new_idx;
        state.active_len = tail;
        state.active_flush_pos = new_flush_pos;
        state.flushing = Some(InFlight::Slot(old_idx));
        drop(state);

        // SAFETY: `flushing = Some(Slot(old_idx))` tells every
        // other thread to leave `bufs[old_idx]` alone; we have
        // exclusive read access for the syscall.
        //
        // 0.9.6 — when iouring is available, submit via
        // `IORING_OP_WRITE_FIXED` against the pre-registered slot
        // index (`old_idx`); the kernel skips per-SQE buffer page
        // pinning. Otherwise fall back to the pwrite path.
        let flush_result = unsafe {
            let slice = (*self.bufs[old_idx as usize].get()).as_slice();
            self.flush_slot_to_disk(file, old_idx, &slice[..write_len], old_flush_pos)
        };

        // Re-acquire, zero the just-flushed slot, and wake any
        // appenders parked on `flush_done`.
        let mut state = self.state.lock();
        // SAFETY: `state.flushing` is still `Some(Slot(old_idx))`;
        // we are still the exclusive owner of `bufs[old_idx]`.
        // Bytes past `old_len` are already zero (module invariant).
        unsafe {
            let slice = (*self.bufs[old_idx as usize].get()).as_mut_slice();
            slice[..old_len].fill(0);
        }
        if let Err(e) = &flush_result {
            poison.set(e);
        }
        state.flushing = None;
        let _ = self.flush_done.notify_all();
        drop(state);

        flush_result
    }

    /// Writes a record that does not fit in a slot. Caller holds
    /// the state lock (passed in as `state`) with no write in
    /// flight.
    ///
    /// The scratch buffer holds the active slot's current
    /// contents followed by the frame, so the write starts at the
    /// sector-aligned `active_flush_pos`. Before the lock is
    /// dropped, the active slot is re-seated at the last sector
    /// boundary of the record and primed with its partial
    /// trailing sector, so concurrent appenders continue right
    /// after the record while the write runs. A failed write
    /// poisons `poison` (it also carried earlier acknowledged
    /// records from the active slot).
    fn append_oversize(
        &self,
        mut state: MutexGuard<'_, State>,
        file: &File,
        payload: &[u8],
        encoded: Option<&[u8]>,
        frame_size: usize,
        poison: &Poison,
    ) -> Result<(u64, u64)> {
        debug_assert!(state.flushing.is_none());
        let ss = self.sector_size;
        let prefix = state.active_len;
        let start_pos = state.active_flush_pos;
        let record_start = start_pos + prefix as u64;
        let total = prefix
            .checked_add(frame_size)
            .ok_or_else(|| Error::Io(std::io::Error::other("oversize frame length overflow")))?;
        let whole = total - total % ss;
        let tail = total - whole;
        let new_flush_pos = start_pos
            .checked_add(whole as u64)
            .ok_or_else(|| Error::Io(std::io::Error::other("flush_pos overflow")))?;

        // Fill the scratch buffer before touching the active slot,
        // so an allocation or encode failure leaves the state
        // unchanged.
        let mut scratch = AlignedBuf::new(round_up(total, ss), ss)?;
        match encoded {
            Some(frame) => scratch.as_mut_slice()[prefix..total].copy_from_slice(frame),
            None => {
                let _ =
                    format::encode_frame_into(payload, &mut scratch.as_mut_slice()[prefix..total])?;
            }
        }
        let active_idx = state.active_idx as usize;
        // SAFETY: we hold the state lock and no write is in
        // flight, so the active slot is exclusively ours. The
        // scratch buffer is a separate allocation. After this
        // block the slot holds only the record's partial trailing
        // sector and is zero past it (module invariant).
        unsafe {
            let slot = (*self.bufs[active_idx].get()).as_mut_slice();
            scratch.as_mut_slice()[..prefix].copy_from_slice(&slot[..prefix]);
            slot[..prefix].fill(0);
            slot[..tail].copy_from_slice(&scratch.as_slice()[whole..total]);
        }
        state.active_len = tail;
        state.active_flush_pos = new_flush_pos;
        state.flushing = Some(InFlight::Scratch);
        drop(state);

        let write_result = crate::platform::write_at_direct(file, start_pos, scratch.as_slice());

        let mut state = self.state.lock();
        if let Err(e) = &write_result {
            poison.set(e);
        }
        state.flushing = None;
        let _ = self.flush_done.notify_all();
        drop(state);

        write_result?;
        Ok((record_start, record_start + frame_size as u64))
    }

    /// Partial / sync-point flush. Writes `aligned_len(active_len)`
    /// bytes at `active_flush_pos` (records + zero-pad to the next
    /// sector boundary). Does NOT advance `active_flush_pos` or
    /// reset `active_len` — the next append continues filling the
    /// active slot from `active_len`. Subsequent flushes overwrite
    /// the partial-sector pad with new record bytes.
    ///
    /// Returns the end LSN of the last record covered by the
    /// write, captured under the state lock. Every byte below it
    /// has been handed to the kernel when this returns `Ok`, so it
    /// is the frontier a following `fdatasync` makes durable.
    ///
    /// **Coordination.** This method waits for any in-flight
    /// write (`flushing.is_some()`) to complete before issuing the
    /// partial flush, then holds the state lock through the
    /// partial-flush syscall. This is the deliberate sync point —
    /// callers asked for "make this durable now" and we honour
    /// that by serialising. Other appenders wait briefly.
    ///
    /// # Errors
    ///
    /// - The poison error if `poison` is set (including by an
    ///   in-flight flush this call waited for).
    /// - [`Error::Io`] if the write fails; `poison` is set.
    pub(crate) fn flush_partial(&self, file: &File, poison: &Poison) -> Result<u64> {
        let mut state = self.state.lock();

        // Wait for any in-flight write to finish. We need a clean
        // state before issuing the partial flush so the on-disk
        // byte sequence is consistent.
        while state.flushing.is_some() {
            self.flush_done.wait(&mut state);
        }
        poison.check()?;

        let end = state.active_flush_pos + state.active_len as u64;
        if state.active_len == 0 {
            return Ok(end); // nothing buffered
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
        let result = unsafe {
            let slice = (*self.bufs[active_idx].get()).as_slice();
            self.flush_slot_to_disk(file, active_idx as u8, &slice[..aligned], active_flush_pos)
        };
        if let Err(e) = &result {
            poison.set(e);
        }
        result.map(|()| end)
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
    use crate::journal::reader::{JournalReader, JournalTailState};
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    static C: AtomicU32 = AtomicU32::new(0);

    /// Shared never-poisoned state for tests that do not inject
    /// failures. Tests that poison use their own instance.
    static NO_POISON: Poison = Poison::new();

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

    fn file_len(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    /// Reads every record back, asserting a clean end.
    fn read_all(path: &Path) -> Vec<(u64, Vec<u8>)> {
        let mut reader = JournalReader::open(path).unwrap();
        let out = reader
            .iter()
            .map(|r| {
                let r = r.unwrap();
                (r.lsn.as_u64(), r.payload)
            })
            .collect();
        assert_eq!(reader.tail_state(), JournalTailState::CleanEnd);
        out
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
        let (start, end) = buf.append_frame(&file, b"hello", &NO_POISON).unwrap();
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
            let _ = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
        }
        // Before rotation — nothing on disk yet.
        assert_eq!(buf.flushed_through(), 0);
        // 37th append triggers rotation; old slot (slot 0) gets flushed.
        let (start, _) = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
        // The 37th record starts right after the 36th: no gap.
        assert_eq!(start, 36 * 112);
        // The rotation wrote round_up(4032, 512) = 4096 bytes and
        // re-seated the active slot at the last whole sector
        // (3584), carrying the 448-byte partial sector.
        assert_eq!(buf.flushed_through(), 3584);
        assert_eq!(buf.buffered_len(), 448 + 112);
        let mut f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 4096);
    }

    #[test]
    fn flush_partial_writes_records_plus_zero_pad() {
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        let _ = buf.append_frame(&file, b"x", &NO_POISON).unwrap(); // 13-byte frame
        let frontier = buf.flush_partial(&file, &NO_POISON).unwrap();
        assert_eq!(frontier, 13);
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
        let (start, end) = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, 5012);
        // active_flush_pos lands at the last sector boundary ≤ end:
        // floor(5012 / 512) * 512 = 4608.
        assert_eq!(buf.flushed_through(), 4608);
        // The active slot's first sector now holds the partial-
        // sector tail: 5012 - 4608 = 404 bytes.
        assert_eq!(buf.buffered_len(), 404);
        // File on disk is sector-aligned-up: round_up(5012, 512) = 5120.
        assert_eq!(file_len(&path), 5120);
    }

    #[test]
    fn test_rotation_with_partial_sector_keeps_lsns_contiguous() {
        // 1.1.0 advanced flush_pos by the whole slot on rotation,
        // leaving a zero gap after the last record of the slot.
        // 13-byte frames into a 4 KiB slot: 4096 = 13 * 315 + 1,
        // so every rotation used to leave a 1-byte gap that the
        // old reader could not skip.
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        let mut expected_start = 0u64;
        for i in 0..2000u32 {
            let payload = [(i % 251) as u8];
            let (start, end) = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
            assert_eq!(start, expected_start, "record {i} is not contiguous");
            expected_start = end;
        }
        assert_eq!(
            buf.flush_partial(&file, &NO_POISON).unwrap(),
            expected_start
        );
        let records = read_all(&path);
        assert_eq!(records.len(), 2000);
        for (i, (lsn, payload)) in records.iter().enumerate() {
            assert_eq!(*lsn, i as u64 * 13);
            assert_eq!(payload, &vec![(i % 251) as u8]);
        }
    }

    #[test]
    fn test_rotation_before_large_record_leaves_no_gap() {
        // A 1000-byte record followed by a 3500-byte record that
        // does not fit behind it: the 1.1.0 rotation left a
        // ~3 KiB zero gap here.
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        let (_, e1) = buf.append_frame(&file, &[1u8; 1000], &NO_POISON).unwrap();
        let (s2, e2) = buf.append_frame(&file, &[2u8; 3500], &NO_POISON).unwrap();
        let (s3, e3) = buf.append_frame(&file, b"after", &NO_POISON).unwrap();
        assert_eq!(s2, e1);
        assert_eq!(s3, e2);
        assert_eq!(buf.flush_partial(&file, &NO_POISON).unwrap(), e3);
        let payloads: Vec<Vec<u8>> = read_all(&path).into_iter().map(|(_, p)| p).collect();
        assert_eq!(
            payloads,
            vec![vec![1u8; 1000], vec![2u8; 3500], b"after".to_vec()]
        );
    }

    #[test]
    fn test_oversize_after_partial_slot_includes_prefix() {
        // Oversize record while the active slot holds data that
        // is not sector-aligned: the scratch write must carry the
        // prefix so nothing is lost or overlapped.
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        let (_, e1) = buf.append_frame(&file, &[1u8; 100], &NO_POISON).unwrap();
        let (s2, e2) = buf.append_frame(&file, &[2u8; 6000], &NO_POISON).unwrap();
        let (s3, e3) = buf.append_frame(&file, &[3u8; 10], &NO_POISON).unwrap();
        assert_eq!((s2, s3), (e1, e2));
        assert_eq!(buf.flush_partial(&file, &NO_POISON).unwrap(), e3);
        let payloads: Vec<Vec<u8>> = read_all(&path).into_iter().map(|(_, p)| p).collect();
        assert_eq!(
            payloads,
            vec![vec![1u8; 100], vec![2u8; 6000], vec![3u8; 10]]
        );
    }

    #[test]
    fn test_oversize_payload_rejected_before_allocation() {
        // A payload over FRAME_MAX_PAYLOAD must be refused by the
        // size check before anything is allocated for it. A real
        // 256 MiB payload is too expensive for a unit test, so the
        // size helper `append_frame` calls first is checked here.
        assert!(format::frame_len(format::FRAME_MAX_PAYLOAD as usize + 1).is_err());
        assert_eq!(
            format::frame_len(format::FRAME_MAX_PAYLOAD as usize).unwrap(),
            format::FRAME_MAX_PAYLOAD as usize + format::FRAME_OVERHEAD
        );
    }

    /// FS-J1 regression: an oversize append used to drop the state
    /// lock without reserving its range, so concurrent appenders
    /// were handed the same LSNs and overwrote its bytes. Four
    /// threads, 4 KiB slots, one thread appending 6000-byte
    /// records: every returned range must be disjoint, the ranges
    /// must tile `[0, end)` with no gap, and every record must
    /// read back intact.
    #[test]
    fn test_concurrent_oversize_appends_never_overlap() {
        for round in 0..5 {
            let (path, file, _g) = make_file();
            let buf = Arc::new(LogBuffer::new(4096, 512, 0).unwrap());
            let file = Arc::new(file);
            let mut handles = Vec::new();
            for t in 0..4u8 {
                let buf = Arc::clone(&buf);
                let file = Arc::clone(&file);
                handles.push(std::thread::spawn(move || {
                    let mut out = Vec::new();
                    for i in 0..400u32 {
                        let len = if t == 0 && i % 4 == 0 { 6000 } else { 20 };
                        let mut payload = vec![t; len];
                        payload[..4].copy_from_slice(&i.to_le_bytes());
                        let (s, e) = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
                        out.push((s, e, payload));
                    }
                    out
                }));
            }
            let mut all: Vec<(u64, u64, Vec<u8>)> = handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect();
            let end = buf.flush_partial(&file, &NO_POISON).unwrap();
            all.sort_by_key(|r| r.0);
            let mut cursor = 0u64;
            for (s, e, _) in &all {
                assert_eq!(*s, cursor, "round {round}: gap or overlap at {s}");
                cursor = *e;
            }
            assert_eq!(cursor, end);
            let records = read_all(&path);
            assert_eq!(records.len(), all.len(), "round {round}");
            for ((lsn, payload), (s, _, expected)) in records.iter().zip(all.iter()) {
                assert_eq!(lsn, s);
                assert_eq!(payload, expected, "round {round}: record at {s} corrupted");
            }
        }
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
            let _ = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
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
        // submit M records and every record reads back.
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
                    let _ = buf
                        .append_frame(&file, &payload, &NO_POISON)
                        .expect("append");
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        // Final sync to push the active slot's tail to disk.
        let end = buf.flush_partial(&file, &NO_POISON).expect("partial flush");

        // LSNs are contiguous, so the frontier is exactly the sum
        // of the frame sizes: 8 * 200 * 36 = 57 600 bytes.
        let total_bytes = (n_threads * per_thread * 36) as u64;
        assert_eq!(end, total_bytes);
        assert_eq!(read_all(&path).len(), n_threads * per_thread);
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
            let _ = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
        }
        // 33rd append triggers rotation: slot 0 → flush, slot 1
        // becomes active with the 33rd frame.
        let _ = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
        // Now flush_partial of the active (slot 1, with one
        // frame in it). After this returns, the file holds
        // slot 0's full content (512 bytes) followed by slot 1's
        // partial content (16 bytes + zero pad to 512).
        assert_eq!(buf.flush_partial(&file, &NO_POISON).unwrap(), 33 * 16);
        // 512 (slot 0 rotation flush) + 512 (slot 1 partial pad) = 1024.
        assert_eq!(file_len(&path), 1024);
        assert_eq!(read_all(&path).len(), 33);
    }

    #[test]
    fn back_to_back_rotations_after_sustained_load() {
        // Smoke test: feed enough records to trigger multiple
        // rotations and confirm offsets advance correctly with
        // no panics, no incorrect state, no deadlock.
        let (path, file, _g) = make_file();
        let buf = LogBuffer::new(4096, 512, 0).unwrap();
        let payload = vec![0xDDu8; 100]; // 112-byte frames
        let n = 36 * 5 + 1;
        for _ in 0..n {
            let _ = buf.append_frame(&file, &payload, &NO_POISON).unwrap();
        }
        // LSNs are contiguous across every rotation.
        assert_eq!(buf.next_lsn(), n as u64 * 112);
        // active_flush_pos is the last sector boundary at or below
        // the start of the active slot's carried tail.
        assert_eq!(buf.flushed_through() % 512, 0);
        assert!(buf.flushed_through() <= buf.next_lsn());
        assert_eq!(
            buf.flush_partial(&file, &NO_POISON).unwrap(),
            n as u64 * 112
        );
        assert_eq!(read_all(&path).len(), n);
    }
}
