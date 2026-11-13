use std::ops::Range;
use std::sync::atomic::{
    AtomicU32, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed},
};

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
    /// The indices of the pool's slots on the whole board, one per worker.
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

/// The reloads of the whole server: OPcache is one cache for every pool, so one worker of any pool resets it for each reload.
pub struct Reloads {
    /// The reloads that the master sent.
    sent: AtomicU64,
    /// The highest `sent` value for which an `opcache_reset()` call has returned.
    reset: AtomicU64,
}

/// Master-side. The mmap happens once, pre-fork, so the addresses are identical in every forked child.
/// Callers pass a bounded count: the master derives it from its pool regions, which stop at `SB_MAX_SLOTS`.
pub fn create(nslots: usize) -> std::io::Result<&'static [SharedSlot]> {
    // SAFETY: every field of SharedSlot is an atomic integer, for which zero is valid.
    unsafe { map_zeroed(nslots) }
}

/// Master-side, one time before the first fork, as [`create`].
pub fn create_reloads() -> std::io::Result<&'static Reloads> {
    // SAFETY: both fields of Reloads are atomic integers, for which zero is valid.
    Ok(&unsafe { map_zeroed::<Reloads>(1) }?[0])
}

/// `count` zero-filled values in a shared anonymous mapping.
/// # Safety
/// The all-zero bit pattern must be a valid `T`.
unsafe fn map_zeroed<T>(count: usize) -> std::io::Result<&'static [T]> {
    // SAFETY:
    // MAP_SHARED|MAP_ANONYMOUS is page-aligned and zero-filled, and the mapping is never munmap'd, so the slice is 'static.
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            count * size_of::<T>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(std::slice::from_raw_parts(ptr.cast::<T>(), count))
    }
}

// The record publishes that a reset call has returned: Acquire in reset_due and AcqRel in record_reset, so a worker that skips its call sees the effects of the call that the record names.
impl Reloads {
    /// Master-side, before it signals the pools.
    pub fn add_sent(&self) {
        self.sent.fetch_add(1, Relaxed);
    }

    /// Worker-side, before each app boot and each classic request, at each app end and in the OPcache restart hook: the `sent` value to reset for, or `None` when a reset call for it has returned already.
    pub fn reset_due(&self) -> Option<u64> {
        let sent = self.sent.load(Relaxed);
        (self.reset.load(Acquire) < sent).then_some(sent)
    }

    /// Worker-side, after its `opcache_reset()` call for `sent` has returned.
    pub fn record_reset(&self, sent: u64) {
        self.reset.fetch_max(sent, AcqRel);
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

    #[test]
    fn a_reset_is_due_until_a_reset_call_for_the_last_reload_has_returned() {
        struct Case {
            name: &'static str,
            sent: u64,
            reset_before: u64,
            want_due: Option<u64>,
            /// The value of the record call, None when the case makes no record call.
            record: Option<u64>,
            want_reset_after: u64,
        }
        let cases = [
            Case {
                name: "no reload yet",
                sent: 0,
                reset_before: 0,
                want_due: None,
                record: None,
                want_reset_after: 0,
            },
            Case {
                name: "one reload",
                sent: 1,
                reset_before: 0,
                want_due: Some(1),
                record: Some(1),
                want_reset_after: 1,
            },
            Case {
                name: "the reset of this reload is done",
                sent: 1,
                reset_before: 1,
                want_due: None,
                record: None,
                want_reset_after: 1,
            },
            Case {
                name: "two reloads before a reset",
                sent: 2,
                reset_before: 0,
                want_due: Some(2),
                record: Some(2),
                want_reset_after: 2,
            },
            Case {
                name: "a new reload after a reset",
                sent: 2,
                reset_before: 1,
                want_due: Some(2),
                record: Some(2),
                want_reset_after: 2,
            },
            Case {
                name: "a worker records an older reload after a newer one",
                sent: 2,
                reset_before: 2,
                want_due: None,
                record: Some(1),
                want_reset_after: 2,
            },
        ];
        for case in cases {
            let reloads = Reloads {
                sent: AtomicU64::new(case.sent),
                reset: AtomicU64::new(case.reset_before),
            };
            let due = reloads.reset_due();
            if let Some(sent) = case.record {
                reloads.record_reset(sent);
            }
            assert_eq!(
                (due, reloads.reset.load(Relaxed)),
                (case.want_due, case.want_reset_after),
                "{}",
                case.name
            );
        }
    }
}
