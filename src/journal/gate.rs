//! In-flight write tracking for buffered-mode journals (1.1.1).
//!
//! A buffered append reserves its LSN range with one atomic
//! `fetch_add` on `next_lsn` and then issues the positioned write
//! without a lock. `next_lsn` is therefore a *reservation*
//! frontier: bytes below it may still be in a caller's buffer. A
//! group-commit leader must not publish a durable frontier that
//! covers a reservation whose write has not reached the file yet,
//! or a slow appender's later `sync_through` would take the
//! "already durable" fast path without any fsync covering its
//! bytes.
//!
//! [`WriteGate`] closes that window with two in-flight counters
//! selected by an epoch parity:
//!
//! - An appender reads the epoch, increments the counter for that
//!   parity, re-reads the epoch to confirm it did not change
//!   (retrying if it did), reserves its LSN range, writes, and
//!   decrements the counter ([`WriteTicket`] does the decrement
//!   on drop).
//! - A leader reads the reservation frontier, flips the epoch,
//!   and waits for the old parity's counter to drain. Every
//!   reservation below the frontier was made by an appender
//!   registered in the old epoch (it confirmed the epoch before
//!   reserving, and reserved before the leader read the
//!   frontier), so once the counter is zero every byte below the
//!   frontier has been handed to the kernel.
//!
//! New appenders register under the new parity, so the wait is
//! bounded by the slowest write already in flight and cannot be
//! starved. All epoch and counter operations, and the reservation
//! itself, use `SeqCst` so the argument above holds in the single
//! total order of those operations. Callers that flip the epoch
//! are serialised by an internal mutex.
//!
//! The gate can also be *closed* ([`WriteGate::close`]): new
//! appenders wait before reserving and the closer waits for every
//! in-flight write to finish. Journal preallocation uses this to
//! restore the file length after a zero-filling fallback without
//! racing an append that extends the file.

use crossbeam_utils::CachePadded;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Epoch word bit set while the gate is closed.
const CLOSED: u64 = 1;
/// Epoch word increment that flips the parity (bit 1).
const FLIP: u64 = 2;

/// In-flight write tracker. See the module docs for the protocol.
pub(crate) struct WriteGate {
    /// Bit 0: [`CLOSED`]. Bits 1..: flip count; bit 1 is the
    /// parity that selects the counter new appenders use.
    epoch: CachePadded<AtomicU64>,
    /// Appenders currently between registration and the end of
    /// their write, per parity.
    in_flight: [CachePadded<AtomicU64>; 2],
    /// Serialises [`Self::drain_below`] and [`Self::close`].
    flipper: Mutex<()>,
}

/// Registration of one in-flight buffered write. Dropping it
/// marks the write finished (successful or not).
#[must_use = "dropping the ticket marks the write finished"]
pub(crate) struct WriteTicket<'a> {
    gate: &'a WriteGate,
    parity: usize,
}

impl Drop for WriteTicket<'_> {
    fn drop(&mut self) {
        self.gate.leave(self.parity);
    }
}

/// A [`WriteTicket`] detached from its borrow of the gate, for a
/// write that outlives the caller's stack frame (the native async
/// append, whose write is owned by the io_uring driver). The owner
/// must hand it back through [`WriteGate::leave_detached`] exactly
/// once, when the write has finished; a detached ticket that is
/// never returned keeps every later drain of its parity waiting.
#[cfg(all(target_os = "linux", feature = "async"))]
#[must_use = "a detached ticket must be returned with WriteGate::leave_detached"]
pub(crate) struct DetachedTicket {
    parity: usize,
}

#[cfg(all(target_os = "linux", feature = "async"))]
impl WriteTicket<'_> {
    /// Detaches the registration from this borrow. See
    /// [`DetachedTicket`].
    pub(crate) fn detach(self) -> DetachedTicket {
        let parity = self.parity;
        // The registration now belongs to the detached ticket; skip
        // this ticket's `Drop`, which would end it.
        std::mem::forget(self);
        DetachedTicket { parity }
    }
}

/// Keeps the gate closed until dropped.
#[must_use = "dropping the guard reopens the gate"]
pub(crate) struct ClosedGate<'a> {
    gate: &'a WriteGate,
    _flipper: parking_lot::MutexGuard<'a, ()>,
}

impl Drop for ClosedGate<'_> {
    fn drop(&mut self) {
        let _ = self.gate.epoch.fetch_and(!CLOSED, Ordering::SeqCst);
    }
}

impl WriteGate {
    pub(crate) fn new() -> Self {
        Self {
            epoch: CachePadded::new(AtomicU64::new(0)),
            in_flight: [
                CachePadded::new(AtomicU64::new(0)),
                CachePadded::new(AtomicU64::new(0)),
            ],
            flipper: Mutex::new(()),
        }
    }

    /// Registers an in-flight write. Must be called **before** the
    /// LSN reservation, and the reservation must use
    /// `Ordering::SeqCst`. Waits while the gate is closed.
    #[inline]
    pub(crate) fn enter(&self) -> WriteTicket<'_> {
        let mut spins = 0u32;
        loop {
            if let Some(ticket) = self.try_enter() {
                return ticket;
            }
            backoff(&mut spins);
        }
    }

    /// [`Self::enter`] without the wait: returns `None` while the
    /// gate is closed. Async callers use it to yield instead of
    /// blocking their worker thread.
    #[inline]
    pub(crate) fn try_enter(&self) -> Option<WriteTicket<'_>> {
        loop {
            let epoch = self.epoch.load(Ordering::SeqCst);
            if epoch & CLOSED != 0 {
                return None;
            }
            let parity = parity(epoch);
            let _ = self.in_flight[parity].fetch_add(1, Ordering::SeqCst);
            if self.epoch.load(Ordering::SeqCst) == epoch {
                return Some(WriteTicket { gate: self, parity });
            }
            // A leader flipped the epoch (or the gate closed)
            // between the read and the increment; we have not
            // reserved anything yet.
            self.leave(parity);
        }
    }

    /// Ends one registration under `parity`.
    #[inline]
    fn leave(&self, parity: usize) {
        let _ = self.in_flight[parity].fetch_sub(1, Ordering::Release);
    }

    /// Ends the registration held by a detached ticket. Call it only
    /// once the ticket's write has finished (successfully or not).
    #[cfg(all(target_os = "linux", feature = "async"))]
    #[inline]
    pub(crate) fn leave_detached(&self, ticket: DetachedTicket) {
        self.leave(ticket.parity);
    }

    /// Reads the reservation frontier with `read_frontier` and
    /// returns it once every write reserved below it has finished.
    /// `read_frontier` must load the reservation counter with
    /// `Ordering::SeqCst`.
    pub(crate) fn drain_below(&self, read_frontier: impl FnOnce() -> u64) -> u64 {
        let _flipper = self.flipper.lock();
        let frontier = read_frontier();
        let old = self.epoch.fetch_add(FLIP, Ordering::SeqCst);
        wait_zero(&self.in_flight[parity(old)]);
        frontier
    }

    /// Non-blocking check for callers that must not wait (the native
    /// async group-commit leader): reads the reservation frontier
    /// with `read_frontier` and returns it when no write is
    /// registered at all after the read, `None` otherwise.
    ///
    /// Sound without an epoch flip: a write reserved below the
    /// frontier registered before its reservation, which precedes
    /// the frontier read in the `SeqCst` order, so a later `SeqCst`
    /// load that reads zero from its counter is ordered after its
    /// decrement and therefore after its write. The check can fail
    /// indefinitely under steady append load; callers then fall back
    /// to [`Self::drain_below`], which cannot be starved.
    #[cfg(all(target_os = "linux", feature = "async"))]
    pub(crate) fn try_quiescent(&self, read_frontier: impl FnOnce() -> u64) -> Option<u64> {
        let frontier = read_frontier();
        let idle = self.in_flight[0].load(Ordering::SeqCst) == 0
            && self.in_flight[1].load(Ordering::SeqCst) == 0;
        idle.then_some(frontier)
    }

    /// Closes the gate: new appenders wait in [`Self::enter`] and
    /// this call returns once every in-flight write has finished.
    /// The gate reopens when the returned guard drops.
    pub(crate) fn close(&self) -> ClosedGate<'_> {
        let flipper = self.flipper.lock();
        let _ = self.epoch.fetch_or(CLOSED, Ordering::SeqCst);
        wait_zero(&self.in_flight[0]);
        wait_zero(&self.in_flight[1]);
        ClosedGate {
            gate: self,
            _flipper: flipper,
        }
    }
}

#[inline]
fn parity(epoch: u64) -> usize {
    ((epoch >> 1) & 1) as usize
}

fn wait_zero(counter: &AtomicU64) {
    let mut spins = 0u32;
    while counter.load(Ordering::Acquire) != 0 {
        backoff(&mut spins);
    }
}

/// Spin briefly, then yield, then sleep: in-flight writes are
/// page-cache copies (microseconds) in the common case but can
/// block on IO.
fn backoff(spins: &mut u32) {
    if *spins < 64 {
        std::hint::spin_loop();
    } else if *spins < 256 {
        std::thread::yield_now();
    } else {
        std::thread::sleep(Duration::from_micros(50));
    }
    *spins = spins.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn test_gate_drain_without_writers_returns_frontier() {
        let gate = WriteGate::new();
        assert_eq!(gate.drain_below(|| 42), 42);
        assert_eq!(gate.drain_below(|| 43), 43);
    }

    #[test]
    fn test_gate_drain_waits_for_registered_writer() {
        let gate = Arc::new(WriteGate::new());
        let ticket_held = Arc::new(AtomicBool::new(true));
        let ticket = gate.enter();
        let g2 = Arc::clone(&gate);
        let held = Arc::clone(&ticket_held);
        let drainer = std::thread::spawn(move || {
            let _ = g2.drain_below(|| 7);
            // The drain may only finish after the ticket dropped.
            assert!(
                !held.load(Ordering::SeqCst),
                "drain finished while a write was in flight"
            );
        });
        std::thread::sleep(Duration::from_millis(50));
        ticket_held.store(false, Ordering::SeqCst);
        drop(ticket);
        drainer.join().unwrap();
    }

    #[test]
    fn test_gate_drain_ignores_writers_registered_after_flip() {
        // A writer that registers after the flip is not waited for,
        // so a continuous stream of new writers cannot starve a
        // leader.
        let gate = Arc::new(WriteGate::new());
        let stop = Arc::new(AtomicBool::new(false));
        let mut writers = Vec::new();
        for _ in 0..4 {
            let gate = Arc::clone(&gate);
            let stop = Arc::clone(&stop);
            writers.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _t = gate.enter();
                    std::thread::sleep(Duration::from_micros(200));
                }
            }));
        }
        let start = Instant::now();
        for _ in 0..50 {
            let _ = gate.drain_below(|| 0);
        }
        assert!(start.elapsed() < Duration::from_secs(10), "drain starved");
        stop.store(true, Ordering::Relaxed);
        for w in writers {
            w.join().unwrap();
        }
    }

    #[test]
    fn test_gate_close_blocks_new_writers_until_reopened() {
        let gate = Arc::new(WriteGate::new());
        let closed = gate.close();
        let entered = Arc::new(AtomicBool::new(false));
        let g2 = Arc::clone(&gate);
        let e2 = Arc::clone(&entered);
        let writer = std::thread::spawn(move || {
            let _t = g2.enter();
            e2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !entered.load(Ordering::SeqCst),
            "writer entered a closed gate"
        );
        drop(closed);
        writer.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
    }

    #[test]
    fn test_gate_close_waits_for_in_flight_writer() {
        let gate = Arc::new(WriteGate::new());
        let ticket = gate.enter();
        let done = Arc::new(AtomicBool::new(false));
        let g2 = Arc::clone(&gate);
        let d2 = Arc::clone(&done);
        let closer = std::thread::spawn(move || {
            let _c = g2.close();
            d2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !done.load(Ordering::SeqCst),
            "close returned with a write in flight"
        );
        drop(ticket);
        closer.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn test_gate_try_enter_on_closed_gate_returns_none() {
        let gate = WriteGate::new();
        {
            let _closed = gate.close();
            assert!(gate.try_enter().is_none());
        }
        let ticket = gate.try_enter().expect("open gate admits a writer");
        drop(ticket);
        assert_eq!(gate.drain_below(|| 5), 5);
    }

    #[cfg(all(target_os = "linux", feature = "async"))]
    #[test]
    fn test_gate_detached_ticket_holds_drain_until_returned() {
        let gate = Arc::new(WriteGate::new());
        let detached = gate.enter().detach();
        assert_eq!(gate.try_quiescent(|| 9), None);
        let finished = Arc::new(AtomicBool::new(false));
        let g2 = Arc::clone(&gate);
        let f2 = Arc::clone(&finished);
        let drainer = std::thread::spawn(move || {
            let frontier = g2.drain_below(|| 9);
            f2.store(true, Ordering::SeqCst);
            frontier
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !finished.load(Ordering::SeqCst),
            "drain finished while a detached write was in flight"
        );
        gate.leave_detached(detached);
        assert_eq!(drainer.join().unwrap(), 9);
        assert_eq!(gate.try_quiescent(|| 11), Some(11));
    }

    #[cfg(all(target_os = "linux", feature = "async"))]
    #[test]
    fn test_gate_try_quiescent_without_writers_returns_frontier() {
        let gate = WriteGate::new();
        assert_eq!(gate.try_quiescent(|| 0), Some(0));
        let ticket = gate.enter();
        assert_eq!(gate.try_quiescent(|| 3), None);
        drop(ticket);
        assert_eq!(gate.try_quiescent(|| 3), Some(3));
    }

    /// Model check of the protocol: writers reserve ranges on a
    /// shared counter and mark them written; a drainer reads the
    /// frontier and, after the drain, every range below it must be
    /// marked written.
    #[test]
    fn test_gate_concurrent_reservations_all_written_below_frontier() {
        use std::sync::Barrier;
        const WRITERS: usize = 6;
        const PER_WRITER: u64 = 3000;
        let gate = Arc::new(WriteGate::new());
        let next = Arc::new(AtomicU64::new(0));
        let written: Arc<Vec<AtomicBool>> = Arc::new(
            (0..WRITERS as u64 * PER_WRITER)
                .map(|_| AtomicBool::new(false))
                .collect(),
        );
        let done = Arc::new(AtomicU64::new(0));
        let start = Arc::new(Barrier::new(WRITERS + 1));
        let mut threads = Vec::new();
        for _ in 0..WRITERS {
            let (gate, next, written, done, start) = (
                Arc::clone(&gate),
                Arc::clone(&next),
                Arc::clone(&written),
                Arc::clone(&done),
                Arc::clone(&start),
            );
            threads.push(std::thread::spawn(move || {
                let _ = start.wait();
                for _ in 0..PER_WRITER {
                    let _t = gate.enter();
                    let slot = next.fetch_add(1, Ordering::SeqCst);
                    if slot % 7 == 0 {
                        std::thread::yield_now();
                    }
                    written[slot as usize].store(true, Ordering::Release);
                }
                let _ = done.fetch_add(1, Ordering::SeqCst);
            }));
        }
        let _ = start.wait();
        let mut checks = 0u64;
        loop {
            let finished = done.load(Ordering::SeqCst) == WRITERS as u64;
            let frontier = gate.drain_below(|| next.load(Ordering::SeqCst));
            for i in 0..frontier {
                assert!(
                    written[i as usize].load(Ordering::Acquire),
                    "slot {i} below frontier {frontier} not written"
                );
            }
            checks += 1;
            if finished {
                assert_eq!(frontier, WRITERS as u64 * PER_WRITER);
                break;
            }
        }
        for t in threads {
            t.join().unwrap();
        }
        assert!(checks > 0);
    }
}
