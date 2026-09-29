use std::cell::Cell;
use std::sync::atomic::Ordering::{Relaxed, Release};

use rapira_scoreboard::{
    SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SLOT_STARTING, SharedSlot, now_millis,
};

thread_local! {
    pub static SB: Cell<Option<&'static SharedSlot>> = const { Cell::new(None) };
    static DRAINING: Cell<bool> = const { Cell::new(false) };
}

pub enum Event {
    Handled(bool),
    Shed,
    Recycled,
    Unhealthy,
    Idle,
    Active,
    Draining,
    /// A boot cycle ended without a pull of the app. The slot shows STARTING until the app pulls.
    BootFailed,
}

pub fn sb_set(slot: &'static SharedSlot) {
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
        Event::Handled(errored) => {
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
        Event::Unhealthy => crate::quota::fire_unhealthy(),
        Event::Idle => {
            let state = if DRAINING.get() {
                SLOT_DRAINING
            } else {
                SLOT_IDLE
            };
            s.state.store(state, Release);
        }
        Event::Active => {
            s.last_activity_ms.store(now_millis(), Relaxed);
            s.state.store(SLOT_ACTIVE, Release);
        }
        Event::Draining => DRAINING.set(true),
        Event::BootFailed => s.state.store(SLOT_STARTING, Release),
    }
}
