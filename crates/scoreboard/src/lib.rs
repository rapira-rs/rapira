use std::ops::Range;
use std::sync::atomic::{
    AtomicU32, AtomicU64,
    Ordering::{Relaxed, Release},
};

pub const SB_MAX_SLOTS: usize = 4096;

pub const SLOT_FREE: u32 = 0;
pub const SLOT_STARTING: u32 = 1;
pub const SLOT_IDLE: u32 = 2;
pub const SLOT_ACTIVE: u32 = 3;
pub const SLOT_DRAINING: u32 = 4; // worker-initiated exit pending

/// Single-writer slot: only its worker mutates it, the master writes just the STARTING/FREE transitions.
#[repr(C, align(64))]
pub struct SharedSlot {
    pub state: AtomicU32,
    pub pid: AtomicU32,
    pub handled: AtomicU64,
    pub errors: AtomicU64,
    pub recycles: AtomicU64,
    pub unhealthy: AtomicU32,
    _pad: [u8; 4],
    pub last_activity_ms: AtomicU64,
    _tail: [u8; 16],
}

const _: () = assert!(size_of::<SharedSlot>() == 64 && align_of::<SharedSlot>() == 64);

/// Copy view over the mapping. The mmap happens once, pre-fork, so the addresses are identical in every forked child.
#[derive(Clone, Copy)]
pub struct Scoreboard {
    slots: &'static [SharedSlot],
}

#[derive(Debug, Default, Clone)]
pub struct SlotSnapshot {
    pub id: usize,
    pub pid: u32,
    pub state: u32,
    pub handled: u64,
    pub errors: u64,
    pub recycles: u64,
    pub unhealthy: bool,
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

    /// Master-side at fork time. It reserves the slot so the scaling math sees the in-flight fork.
    pub fn set_starting(&self, i: usize) {
        let s = self.slot(i);
        s.last_activity_ms.store(now_millis(), Relaxed);
        s.state.store(SLOT_STARTING, Release);
    }

    /// Master-side, after the slot's worker is reaped. The slot can then go to a new fork.
    pub fn clear(&self, i: usize) {
        let s = self.slot(i);
        s.pid.store(0, Relaxed);
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
                unhealthy: s.unhealthy.load(Relaxed) != 0,
            })
            .collect()
    }
}

impl SharedSlot {
    /// Worker-side claim and reset. It runs exactly once per process before requests flow.
    pub fn bind(&'static self, pid: u32) {
        self.handled.store(0, Relaxed);
        self.errors.store(0, Relaxed);
        self.recycles.store(0, Relaxed);
        self.unhealthy.store(0, Relaxed);
        self.pid.store(pid, Relaxed);
        self.last_activity_ms.store(now_millis(), Relaxed);
        self.state.store(SLOT_IDLE, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_bind_snapshot_roundtrip() {
        let sb = Scoreboard::create(3).unwrap();
        assert_eq!(sb.nslots(), 3);
        assert_eq!(sb.slot(0).state.load(Relaxed), SLOT_FREE);

        sb.set_starting(0);
        assert_eq!(sb.slot(0).state.load(Relaxed), SLOT_STARTING);
        assert_eq!(sb.slot(1).state.load(Relaxed), SLOT_FREE);

        let slot = sb.slot(0);
        slot.bind(4242);
        slot.handled.fetch_add(2, Relaxed);
        slot.errors.fetch_add(1, Relaxed);

        let snap = sb.snapshot_slots();
        assert_eq!(snap.len(), 1);
        assert_eq!((snap[0].pid, snap[0].handled, snap[0].errors), (4242, 2, 1));
        assert_eq!(snap[0].state, SLOT_IDLE);

        sb.clear(0);
        assert_eq!(sb.slot(0).state.load(Relaxed), SLOT_FREE);
        assert!(sb.snapshot_slots().is_empty());
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
