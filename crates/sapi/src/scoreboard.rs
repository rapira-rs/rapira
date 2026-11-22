use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Relaxed, Release, SeqCst};

use rapira_scoreboard::{
    Board, Pool, SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SLOT_STARTING, Worker, now_millis,
};

thread_local! {
    static SB: RefCell<Option<(Arc<Pool>, usize)>> = const { RefCell::new(None) };
}

static DRAINING: AtomicBool = AtomicBool::new(false);

pub enum Event {
    Handled { errored: bool },
    Shed,
    Recycled,
    Idle,
    Active,
    Starting,
    Stopped,
}

pub(crate) fn sb_set(stats: Arc<Pool>, index: usize) {
    SB.set(Some((stats, index)));
}

pub fn sb_update(event: Event) {
    SB.with_borrow(|slot| {
        let Some((pool, index)) = slot else { return };
        let s = &pool.workers[*index];
        if matches!(
            event,
            Event::Idle | Event::Active | Event::Recycled | Event::Handled { errored: false }
        ) && !pool.served.load(Relaxed)
        {
            pool.served.store(true, Release);
        }
        match event {
            Event::Handled { errored } => {
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
                inactive(s, SLOT_STARTING);
            }
            Event::Idle => inactive(s, SLOT_IDLE),
            Event::Active => {
                let since = now_millis();
                crate::watchdog::arm(since);
                s.last_activity_ms.store(since, Relaxed);
                s.state.store(SLOT_ACTIVE, Release);
            }
            Event::Starting => inactive(s, SLOT_STARTING),
            Event::Stopped => {
                crate::watchdog::clear();
                s.state.store(SLOT_DRAINING, Release);
            }
        }
    });
}

/// Workers with a request in progress stay active until their next pull or shutdown.
pub fn begin_drain(board: &Board) {
    DRAINING.store(true, SeqCst);
    for pool in board.iter() {
        for s in &pool.workers {
            let _ = s.state.try_update(SeqCst, SeqCst, |state| {
                (state != SLOT_ACTIVE).then_some(SLOT_DRAINING)
            });
        }
    }
}

pub(crate) fn draining() -> bool {
    DRAINING.load(SeqCst)
}

/// Store first so a concurrent drain sees this state or this thread sees the drain.
fn inactive(worker: &Worker, state: u32) {
    crate::watchdog::clear();
    worker.state.store(state, SeqCst);
    if draining() {
        worker.state.store(SLOT_DRAINING, Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request stays active during the drain until its next pull.
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
        let pool = Arc::new(Pool::new("http", 1));
        let board: Board = Arc::from([Arc::clone(&pool)]);
        let slot = &pool.workers[0];
        sb_set(Arc::clone(&pool), 0);
        for case in cases {
            DRAINING.store(case.draining, Relaxed);
            slot.state.store(case.state, Relaxed);
            match case.step {
                Step::Drain => begin_drain(&board),
                Step::Idle => sb_update(Event::Idle),
                Step::BootFailed => sb_update(Event::Starting),
            }
            assert_eq!(slot.state.load(Relaxed), case.want, "{}", case.name);
        }
        DRAINING.store(false, SeqCst);
    }
}
