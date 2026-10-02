use std::ops::Range;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

pub const SB_MAX_SLOTS: usize = 4096;

pub const SLOT_FREE: u32 = 0;
pub const SLOT_STARTING: u32 = 1;
pub const SLOT_IDLE: u32 = 2;
pub const SLOT_ACTIVE: u32 = 3;
pub const SLOT_DRAINING: u32 = 4; // worker-initiated exit pending

/// Each field has one writer at a time. The worker writes the IDLE, ACTIVE and DRAINING states, the STARTING state after a failed boot cycle, the request counters, `pending` and `failed_on_full_queue`. The master writes the STARTING and FREE states, and `pid` at spawn and at clear. It writes `pending` and the exit counters only while no worker owns the slot: after the reap and before the next spawn.
#[repr(C, align(64))]
pub struct SharedSlot {
    pub state: AtomicU32,
    pub pid: AtomicU32,
    pub handled: AtomicU64,
    pub errors: AtomicU64,
    pub recycles: AtomicU64,
    /// [`now_millis`] when the worker last went ACTIVE. The request watchdog measures the request age from it.
    pub last_activity_ms: AtomicU64,
    /// Units that the IO runtime handed to the worker queue, or that wait for room in a full queue, and that the PHP thread has not pulled yet.
    pub pending: AtomicU64,
    /// Worker exits per verdict. The master counts them.
    pub exits_drained: AtomicU64,
    pub exits_recycled: AtomicU64,
    pub exits_unhealthy: AtomicU64,
    pub exits_timeout: AtomicU64,
    pub exits_crashed: AtomicU64,
    /// Units that found the worker queue full and never entered it. An IO thread of the worker counts them.
    pub failed_on_full_queue: AtomicU64,
}

const _: () = assert!(size_of::<SharedSlot>() == 128 && align_of::<SharedSlot>() == 64);

/// Copy view over the mapping. The mmap happens once, pre-fork, so the addresses are identical in every forked child.
#[derive(Clone, Copy)]
pub struct Scoreboard {
    slots: &'static [SharedSlot],
}

/// The part of the board that one pool owns.
#[derive(Debug, PartialEq)]
pub struct PoolRegion {
    /// The config table of the pool ("http").
    pub name: &'static str,
    /// The worker count of the pool.
    pub processes: usize,
    /// The indices of the pool's slots on the whole board.
    pub slots: Range<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct SlotSnapshot {
    pub id: usize,
    pub pid: u32,
    pub state: u32,
    pub handled: u64,
    pub errors: u64,
    pub recycles: u64,
}

/// Milliseconds on `CLOCK_MONOTONIC`. The values compare across processes within one boot. Wall-clock steps do not move them.
pub fn now_millis() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a live out-param; CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

impl Scoreboard {
    /// Master-side, pre-fork. The mapping must exist before a fork can inherit it.
    /// Callers pass a bounded count: the master derives it from its pool regions, which stop at `SB_MAX_SLOTS`.
    pub fn create(nslots: usize) -> anyhow::Result<Scoreboard> {
        let bytes = nslots * size_of::<SharedSlot>();
        // SAFETY:
        // MAP_SHARED|MAP_ANONYMOUS is page-aligned and zero-filled (a valid bit pattern for every field), and the mapping is never munmap'd, so the slice is 'static.
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            anyhow::ensure!(
                ptr != libc::MAP_FAILED,
                "scoreboard mmap failed: {}",
                std::io::Error::last_os_error()
            );
            let slots = std::slice::from_raw_parts(ptr.cast::<SharedSlot>(), nslots);
            Ok(Scoreboard { slots })
        }
    }

    /// View over `range` of this board: indices inside the view are local to it, the memory is shared.
    pub fn slice(&self, range: Range<usize>) -> Scoreboard {
        Scoreboard {
            slots: &self.slots[range],
        }
    }

    pub fn nslots(&self) -> usize {
        self.slots.len()
    }

    pub fn slot(&self, i: usize) -> &'static SharedSlot {
        &self.slots[i]
    }

    pub fn slots(&self) -> &'static [SharedSlot] {
        self.slots
    }

    /// Master-side at fork time. It reserves the slot, so the next spawn cannot take it.
    pub fn set_starting(&self, i: usize) {
        self.slot(i).state.store(SLOT_STARTING, Relaxed);
    }

    /// Master-side, after the slot's worker is reaped. The queue of the dead worker is gone, so `pending` goes to 0. The slot can then go to a new fork.
    pub fn clear(&self, i: usize) {
        let s = self.slot(i);
        s.pid.store(0, Relaxed);
        s.pending.store(0, Relaxed);
        s.state.store(SLOT_FREE, Relaxed);
    }

    pub fn snapshot_slots(&self) -> Vec<SlotSnapshot> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state.load(Relaxed) != SLOT_FREE || s.pid.load(Relaxed) != 0)
            .map(|(id, s)| SlotSnapshot {
                id,
                pid: s.pid.load(Relaxed),
                state: s.state.load(Relaxed),
                handled: s.handled.load(Relaxed),
                errors: s.errors.load(Relaxed),
                recycles: s.recycles.load(Relaxed),
            })
            .collect()
    }
}

impl SharedSlot {
    /// Serving is IDLE or ACTIVE: under load a replacement may never be observed IDLE between requests.
    pub fn serving(&self) -> bool {
        matches!(self.state.load(Relaxed), SLOT_IDLE | SLOT_ACTIVE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebound_slot_keeps_its_counts_and_clear_empties_its_queue() {
        let sb = Scoreboard::create(1).unwrap();
        let slot = sb.slot(0);
        sb.set_starting(0);
        slot.pid.store(4242, Relaxed);
        slot.handled.fetch_add(3, Relaxed);
        slot.errors.fetch_add(1, Relaxed);
        slot.recycles.fetch_add(1, Relaxed);
        slot.pending.fetch_add(2, Relaxed);

        sb.clear(0);
        assert_eq!(slot.state.load(Relaxed), SLOT_FREE);
        assert_eq!(slot.pid.load(Relaxed), 0);
        assert_eq!(
            slot.pending.load(Relaxed),
            0,
            "the queue died with the worker"
        );
        assert!(sb.snapshot_slots().is_empty());

        sb.set_starting(0);
        slot.pid.store(4343, Relaxed);
        let snap = sb.snapshot_slots();
        assert_eq!(snap.len(), 1);
        assert_eq!(
            (snap[0].handled, snap[0].errors, snap[0].recycles),
            (3, 1, 1),
            "the counts of the first worker stay in the slot"
        );
    }

    #[test]
    fn slice_shares_memory_with_local_indices() {
        let board = Scoreboard::create(6).unwrap();
        let view = board.slice(2..4);
        assert_eq!(view.nslots(), 2);
        assert!(std::ptr::eq(view.slot(1), board.slot(3)));

        view.set_starting(0);
        assert_eq!(board.slot(2).state.load(Relaxed), SLOT_STARTING);
        assert_eq!(board.slot(1).state.load(Relaxed), SLOT_FREE);

        assert_eq!(view.snapshot_slots()[0].id, 0);
    }
}
