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
    counter: &'a AtomicU64,
}

impl Drop for WriteTicket<'_> {
    fn drop(&mut self) {
        let _ = self.counter.fetch_sub(1, Ordering::Release);
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
            let epoch = self.epoch.load(Ordering::SeqCst);
            if epoch & CLOSED != 0 {
                backoff(&mut spins);
                continue;
            }
            let counter: &AtomicU64 = &self.in_flight[parity(epoch)];
            let _ = counter.fetch_add(1, Ordering::SeqCst);
            if self.epoch.load(Ordering::SeqCst) == epoch {
                return WriteTicket { counter };
            }
            // A leader flipped the epoch (or the gate closed)
            // between the read
            // and the increment; we have not reserved anything yet.
            let _ = counter.fetch_sub(1, Ordering::Release);
        }
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
