//! Worker-side bridge to the shared-memory scoreboard. The slot pointer is
//! thread-local to the single PHP worker thread; every update is an atomic
//! store on the slot this process owns.

use std::cell::Cell;
use std::sync::atomic::Ordering::{Relaxed, Release};

use rapira_scoreboard::{SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SharedSlot, now_millis};

pub use rapira_scoreboard::SlotSnapshot;

thread_local! {
    pub static SB: Cell<Option<&'static SharedSlot>> = const { Cell::new(None) };
    static DRAINING: Cell<bool> = const { Cell::new(false) };
}

pub enum Event {
    Handled(bool),
    Shed, // boot-failure 503: counted on the board, but not a served request
    Recycled,
    Restart,
    Unhealthy,
    Healthy,
    Idle,
    Active,
    Draining,
}

pub fn sb_set(slot: &'static SharedSlot) {
    SB.set(Some(slot));
}

pub fn sb_update(event: Event) {
    let Some(s) = SB.get() else { return };
    match event {
        Event::Handled(errored) => {
            if errored {
                s.errors.fetch_add(1, Relaxed);
            }
            // Release pairs with the master's Acquire load of `handled`: an
            // observed count implies its matching error increment is visible.
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
        Event::Restart => {
            s.restarts.fetch_add(1, Relaxed);
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
            s.state.store(state, Relaxed);
            s.last_activity_ms.store(now_millis(), Relaxed);
        }
        Event::Active => {
            s.state.store(SLOT_ACTIVE, Relaxed);
            s.last_activity_ms.store(now_millis(), Relaxed);
        }
        Event::Draining => DRAINING.set(true),
    }
}

/// Totals kept for `Rapira::scoreboard()` (in-process tests assert on these
/// fields); filled from the shared board's slots.
#[derive(Debug, Default, Clone)]
pub struct ScoreboardSnapshot {
    pub handled: u64,
    pub errors: u64,
    pub recycles: u64,
    pub restarts: u64,
    pub unhealthy: usize, // workers currently flagged unhealthy
    pub workers: Vec<SlotSnapshot>,
}

pub(crate) fn snapshot(board: &rapira_scoreboard::Scoreboard) -> ScoreboardSnapshot {
    let workers = board.snapshot_slots();
    ScoreboardSnapshot {
        handled: workers.iter().map(|w| w.handled).sum(),
        errors: workers.iter().map(|w| w.errors).sum(),
        recycles: workers.iter().map(|w| w.recycles).sum(),
        restarts: workers.iter().map(|w| w.restarts).sum(),
        unhealthy: workers.iter().filter(|w| w.unhealthy).count(),
        workers,
    }
}
