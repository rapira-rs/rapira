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

/// Master-side. The mmap happens once, pre-fork, so the addresses are identical in every forked child.
/// Callers pass a bounded count: the master derives it from its pool regions, which stop at `SB_MAX_SLOTS`.
pub fn create(nslots: usize) -> std::io::Result<&'static [SharedSlot]> {
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
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(std::slice::from_raw_parts(ptr.cast::<SharedSlot>(), nslots))
    }
}

impl SharedSlot {
    /// Serving is IDLE or ACTIVE: under load a replacement may never be observed IDLE between requests.
    pub fn serving(&self) -> bool {
        matches!(self.state.load(Relaxed), SLOT_IDLE | SLOT_ACTIVE)
    }

    /// Master-side at fork time. It reserves the slot, so the next spawn cannot take it.
    pub fn set_starting(&self) {
        self.state.store(SLOT_STARTING, Relaxed);
    }

    /// Master-side, after the slot's worker is reaped. The queue of the dead worker is gone, so `pending` goes to 0. The slot can then go to a new fork.
    pub fn clear(&self) {
        self.pid.store(0, Relaxed);
        self.pending.store(0, Relaxed);
        self.state.store(SLOT_FREE, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebound_slot_keeps_its_counts_and_clear_empties_its_queue() {
        let slot = &create(1).unwrap()[0];
        slot.set_starting();
        slot.pid.store(4242, Relaxed);
        slot.handled.fetch_add(3, Relaxed);
        slot.errors.fetch_add(1, Relaxed);
        slot.recycles.fetch_add(1, Relaxed);
        slot.pending.fetch_add(2, Relaxed);

        slot.clear();
        assert_eq!(slot.state.load(Relaxed), SLOT_FREE);
        assert_eq!(slot.pid.load(Relaxed), 0);
        assert_eq!(
            slot.pending.load(Relaxed),
            0,
            "the queue died with the worker"
        );

        slot.set_starting();
        slot.pid.store(4343, Relaxed);
        assert_eq!(
            (
                slot.handled.load(Relaxed),
                slot.errors.load(Relaxed),
                slot.recycles.load(Relaxed)
            ),
            (3, 1, 1),
            "the counts of the first worker stay in the slot"
        );
    }
}
