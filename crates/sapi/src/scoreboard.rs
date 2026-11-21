use std::cell::Cell;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Relaxed, Release};

use rapira_scoreboard::{
    SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SLOT_STARTING, SharedSlot, now_millis,
};

thread_local! {
    static SB: Cell<Option<&'static SharedSlot>> = const { Cell::new(None) };
}

/// The drain decision. One worker runs in each process, so the signal thread sets the flag that the PHP thread reads.
static DRAINING: AtomicBool = AtomicBool::new(false);

pub enum Event {
    Handled {
        errored: bool,
    },
    Shed,
    Recycled,
    Idle,
    Active,
    Draining,
    /// A boot cycle ends without a pull of the app. The slot shows STARTING until the app pulls. After the drain decision, the slot shows DRAINING.
    BootFailed,
}

pub(crate) fn sb_set(slot: &'static SharedSlot) {
    SB.set(Some(slot));
}

/// Counts a unit that the host shed: one error, then one handled unit. It takes the slot, because only the PHP thread sets `SB`.
pub(crate) fn count_shed(s: &SharedSlot) {
    s.errors.fetch_add(1, Relaxed);
    s.handled.fetch_add(1, Release);
}

/// Each Release store publishes the Relaxed write before it (errors, last_activity_ms) to the master's Acquire load.
pub fn sb_update(event: Event) {
    let Some(s) = SB.get() else { return };
    match event {
        Event::Handled { errored } => {
            if errored {
                s.errors.fetch_add(1, Relaxed);
            }
            s.handled.fetch_add(1, Release);
            crate::quota::tick();
        }
        Event::Shed => count_shed(s),
        Event::Recycled => {
            s.recycles.fetch_add(1, Relaxed);
        }
        Event::Idle => s.state.store(draining_or(SLOT_IDLE), Release),
        Event::Active => {
            s.last_activity_ms.store(now_millis(), Relaxed);
            s.state.store(SLOT_ACTIVE, Release);
        }
        Event::Draining => drain(s),
        Event::BootFailed => s.state.store(draining_or(SLOT_STARTING), Release),
    }
}

/// The drain decision: the slot shows DRAINING, and the later Idle and BootFailed stores keep it. A request in flight keeps ACTIVE for the request watchdog until its Idle store. It takes the slot, because only the PHP thread sets `SB`.
pub fn drain(s: &SharedSlot) {
    DRAINING.store(true, Relaxed);
    let _ = s.state.try_update(Release, Relaxed, |state| {
        (state != SLOT_ACTIVE).then_some(SLOT_DRAINING)
    });
}

/// `state`, or DRAINING after the drain decision.
fn draining_or(state: u32) -> u32 {
    if DRAINING.load(Relaxed) {
        SLOT_DRAINING
    } else {
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot shows draining after the worker decides to exit. A request in flight stays active until its next pull. One test holds all cases, because the drain decision is process-wide.
    #[test]
    fn the_slot_shows_draining_after_the_drain_decision() {
        enum Step {
            Drain,
            Idle,
            BootFailed,
        }
        struct Case {
            name: &'static str,
            draining: bool,
            state: u32,
            step: Step,
            want: u32,
        }
        let cases = [
            Case {
                name: "a failed boot cycle",
                draining: false,
                state: SLOT_STARTING,
                step: Step::BootFailed,
                want: SLOT_STARTING,
            },
            Case {
                name: "a failed boot cycle after the drain decision",
                draining: true,
                state: SLOT_STARTING,
                step: Step::BootFailed,
                want: SLOT_DRAINING,
            },
            Case {
                name: "the drain decision in an idle worker",
                draining: false,
                state: SLOT_IDLE,
                step: Step::Drain,
                want: SLOT_DRAINING,
            },
            Case {
                name: "the drain decision during a request",
                draining: false,
                state: SLOT_ACTIVE,
                step: Step::Drain,
                want: SLOT_ACTIVE,
            },
            Case {
                name: "the next pull after the drain decision",
                draining: true,
                state: SLOT_ACTIVE,
                step: Step::Idle,
                want: SLOT_DRAINING,
            },
        ];
        let slot = &rapira_scoreboard::create(1).unwrap()[0];
        sb_set(slot);
        for case in cases {
            DRAINING.store(case.draining, Relaxed);
            slot.state.store(case.state, Relaxed);
            match case.step {
                Step::Drain => drain(slot),
                Step::Idle => sb_update(Event::Idle),
                Step::BootFailed => sb_update(Event::BootFailed),
            }
            assert_eq!(slot.state.load(Relaxed), case.want, "{}", case.name);
        }
    }
}
