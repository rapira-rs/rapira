use std::cell::Cell;
use std::sync::atomic::Ordering::{Relaxed, Release};

use rapira_scoreboard::{SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SharedSlot, now_millis};

thread_local! {
    pub static SB: Cell<Option<&'static SharedSlot>> = const { Cell::new(None) };
    static DRAINING: Cell<bool> = const { Cell::new(false) };
}

pub enum Event {
    Handled(bool),
    Shed,
    Recycled,
    Unhealthy,
    Healthy,
    Idle,
    Active,
    Draining,
}

pub fn sb_set(slot: &'static SharedSlot) {
    SB.set(Some(slot));
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
        Event::Shed => {
            s.errors.fetch_add(1, Relaxed);
            s.handled.fetch_add(1, Release);
        }
        Event::Recycled => {
            s.recycles.fetch_add(1, Relaxed);
        }
        Event::Unhealthy => {
            s.unhealthy.store(1, Relaxed);
            crate::quota::fire_unhealthy();
        }
        Event::Healthy => s.unhealthy.store(0, Relaxed),
        Event::Idle => {
            let state = if DRAINING.get() {
                SLOT_DRAINING
            } else {
                SLOT_IDLE
            };
            s.last_activity_ms.store(now_millis(), Relaxed);
            s.state.store(state, Release);
        }
        Event::Active => {
            s.last_activity_ms.store(now_millis(), Relaxed);
            s.state.store(SLOT_ACTIVE, Release);
        }
        Event::Draining => DRAINING.set(true),
    }
}
