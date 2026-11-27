//! The record of a pool: its counters, and the state and the counters of each worker.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use crate::work::IntakeLimits;

/// The state of a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The worker boots an app instance.
    Starting,
    /// The worker waits for a unit.
    Idle,
    /// The worker runs the app after a pull.
    Active,
    /// The server stops and the worker is not active, or the worker thread ended.
    Draining,
}

impl State {
    /// Every state in output order. The position of a state is its discriminant.
    pub const ALL: [Self; 4] = [Self::Starting, Self::Idle, Self::Active, Self::Draining];

    /// The text of the state in a status line and in a metric label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Active => "active",
            Self::Draining => "draining",
        }
    }

    /// The worker can serve a unit.
    pub fn serving(self) -> bool {
        matches!(self, Self::Idle | Self::Active)
    }
}

#[repr(align(64))]
#[derive(Default)]
pub(crate) struct CachePadded<T>(pub(crate) T);

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

pub struct Pool {
    pub name: &'static str,
    pub(crate) workers: Box<[Worker]>,
    /// A worker of the pool reached a pull.
    served: AtomicBool,
    /// The server stop started.
    draining: AtomicBool,
    /// Active workers whose interrupted unit is still running after the watchdog grace period.
    pub(crate) blocked: AtomicUsize,
    /// Lock-free body-byte budget and queue-slot accounting, also backing `pending()`.
    pub(crate) limits: IntakeLimits,
    pub(crate) failed_on_full_queue: AtomicU64,
    /// Units refused before a worker could take them.
    pub(crate) refused: AtomicU64,
}

#[repr(align(64))]
#[derive(Default)]
pub(crate) struct Worker {
    /// A `State` discriminant.
    state: AtomicU8,
    succeeded: AtomicU64,
    failed: AtomicU64,
    recycles: AtomicU64,
    last_activity_ms: AtomicU64,
}

/// The values of a pool at one read.
pub struct PoolSnapshot {
    pub name: &'static str,
    pub pending: u64,
    pub failed_on_full_queue: u64,
    pub refused: u64,
    pub workers: Vec<WorkerSnapshot>,
}

pub struct WorkerSnapshot {
    pub state: State,
    /// Units that the worker took. The count includes `errors`.
    pub handled: u64,
    pub errors: u64,
    pub recycles: u64,
    pub last_activity_ms: u64,
}

impl Pool {
    pub(crate) fn new(name: &'static str, workers: usize) -> Self {
        Self {
            name,
            workers: (0..workers).map(|_| Worker::default()).collect(),
            served: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            blocked: AtomicUsize::new(0),
            limits: IntakeLimits::new(),
            failed_on_full_queue: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }
    }

    pub(crate) fn pending(&self) -> u64 {
        self.limits.pending()
    }

    /// Binds the calling thread as worker `index` of this pool.
    pub(crate) fn bind(self: &Arc<Self>, index: usize) {
        BOUND.set(Some(Bound {
            pool: Arc::clone(self),
            index,
        }));
    }

    /// The state that a reader sees. After the drain starts, only an active worker keeps its state.
    pub(crate) fn state(&self, index: usize) -> State {
        let state = State::ALL[usize::from(self.workers[index].state.load(Relaxed))];
        if state != State::Active && self.draining() {
            State::Draining
        } else {
            state
        }
    }

    /// A worker can serve a unit.
    pub fn ready(&self) -> bool {
        (0..self.workers.len()).any(|index| self.state(index).serving())
    }

    pub(crate) fn served(&self) -> bool {
        self.served.load(Relaxed)
    }

    pub(crate) fn draining(&self) -> bool {
        self.draining.load(Relaxed)
    }

    /// A worker with a unit in progress stays active until its next pull.
    pub(crate) fn begin_drain(&self) {
        self.draining.store(true, Relaxed);
    }

    /// Records an event for worker `index` directly without reading thread-local state.
    pub(crate) fn record_worker(&self, index: usize, event: Event) {
        let worker = &self.workers[index];
        if matches!(
            event,
            Event::Idle | Event::Active | Event::Recycled | Event::Handled { errored: false }
        ) && !self.served()
        {
            self.served.store(true, Relaxed);
        }
        match event {
            Event::Handled { errored } => worker.count(errored),
            Event::Shed => worker.count(true),
            Event::Recycled => {
                worker.recycles.fetch_add(1, Relaxed);
                worker.inactive(State::Starting);
            }
            Event::Idle => worker.inactive(State::Idle),
            Event::Active => {
                let since = now_millis();
                crate::watchdog::arm(since);
                worker.last_activity_ms.store(since, Relaxed);
                worker.state.store(State::Active as u8, Relaxed);
            }
            Event::Starting => worker.inactive(State::Starting),
            Event::Stopped => worker.state.store(State::Draining as u8, Relaxed),
        }
    }

    pub fn snapshot(&self) -> PoolSnapshot {
        let mut workers = Vec::with_capacity(self.workers.len());
        for (index, worker) in self.workers.iter().enumerate() {
            // One read of the failed count keeps errors at or below handled.
            let errors = worker.failed.load(Relaxed);
            workers.push(WorkerSnapshot {
                state: self.state(index),
                handled: worker.succeeded.load(Relaxed) + errors,
                errors,
                recycles: worker.recycles.load(Relaxed),
                last_activity_ms: worker.last_activity_ms.load(Relaxed),
            });
        }
        PoolSnapshot {
            name: self.name,
            pending: self.pending(),
            failed_on_full_queue: self.failed_on_full_queue.load(Relaxed),
            refused: self.refused.load(Relaxed),
            workers,
        }
    }
}

impl Worker {
    fn count(&self, failed: bool) {
        let counter = if failed {
            &self.failed
        } else {
            &self.succeeded
        };
        counter.fetch_add(1, Relaxed);
    }

    /// The worker has no unit in progress, so the watchdog has nothing to interrupt.
    fn inactive(&self, state: State) {
        crate::watchdog::clear();
        self.state.store(state as u8, Relaxed);
    }
}

struct Bound {
    pool: Arc<Pool>,
    index: usize,
}

thread_local! {
    /// The pool and the index of the worker that runs on this thread.
    static BOUND: RefCell<Option<Bound>> = const { RefCell::new(None) };
}

pub enum Event {
    Handled { errored: bool },
    Shed,
    Recycled,
    Idle,
    Active,
    Starting,
    Stopped,
}

/// Records an event of the worker that runs on the calling thread.
pub fn record(event: Event) {
    BOUND.with_borrow(|bound| {
        if let Some(Bound { pool, index }) = bound {
            pool.record_worker(*index, event);
        }
    });
}

/// Monotonic milliseconds from the first call in this process.
pub(crate) fn now_millis() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_is_ready_when_any_worker_can_serve() {
        struct Case {
            name: &'static str,
            states: [State; 2],
            ready: bool,
        }
        let cases = [
            Case {
                name: "both starting",
                states: [State::Starting, State::Starting],
                ready: false,
            },
            Case {
                name: "one idle",
                states: [State::Starting, State::Idle],
                ready: true,
            },
            Case {
                name: "one active",
                states: [State::Active, State::Draining],
                ready: true,
            },
            Case {
                name: "last worker draining",
                states: [State::Draining, State::Starting],
                ready: false,
            },
        ];
        for case in cases {
            let pool = Pool::new("http", 2);
            for (worker, state) in pool.workers.iter().zip(case.states) {
                worker.state.store(state as u8, Relaxed);
            }
            assert_eq!(pool.ready(), case.ready, "{}", case.name);
        }
    }

    /// A pool that sheds every request must report the same handled and failed counts in each sample.
    #[test]
    fn a_live_shed_sample_counts_each_outcome_once() {
        const SHED: u64 = 65_536;
        let pool = Arc::new(Pool::new("http", 1));
        let writer = Arc::clone(&pool);
        let go = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let go = &go;
            let worker = scope.spawn(move || {
                writer.bind(0);
                go.wait();
                for _ in 0..SHED {
                    record(Event::Shed);
                }
            });
            go.wait();
            while !worker.is_finished() {
                let snapshot = pool.snapshot();
                let sample = &snapshot.workers[0];
                assert_eq!(sample.handled, sample.errors, "live shed sample");
            }
            worker.join().unwrap();
            let snapshot = pool.snapshot();
            let sample = &snapshot.workers[0];
            assert_eq!((sample.handled, sample.errors), (SHED, SHED));
        });
    }

    /// A request stays active during the drain until its next pull.
    #[test]
    fn a_worker_shows_draining_after_the_drain_decision() {
        enum Step {
            Drain,
            Idle,
            BootFailed,
            Stopped,
        }
        struct Case {
            name: &'static str,
            draining: bool,
            state: State,
            step: Step,
            want: State,
        }
        let cases = [
            Case {
                name: "a failed boot cycle",
                draining: false,
                state: State::Starting,
                step: Step::BootFailed,
                want: State::Starting,
            },
            Case {
                name: "a failed boot cycle after the drain decision",
                draining: true,
                state: State::Starting,
                step: Step::BootFailed,
                want: State::Draining,
            },
            Case {
                name: "the drain decision in an idle worker",
                draining: false,
                state: State::Idle,
                step: Step::Drain,
                want: State::Draining,
            },
            Case {
                name: "the drain decision during a request",
                draining: false,
                state: State::Active,
                step: Step::Drain,
                want: State::Active,
            },
            Case {
                name: "the next pull after the drain decision",
                draining: true,
                state: State::Active,
                step: Step::Idle,
                want: State::Draining,
            },
            Case {
                name: "a worker thread that ended before the drain decision",
                draining: false,
                state: State::Active,
                step: Step::Stopped,
                want: State::Draining,
            },
        ];
        for case in cases {
            let pool = Arc::new(Pool::new("http", 1));
            pool.bind(0);
            pool.workers[0].state.store(case.state as u8, Relaxed);
            if case.draining {
                pool.begin_drain();
            }
            match case.step {
                Step::Drain => pool.begin_drain(),
                Step::Idle => record(Event::Idle),
                Step::BootFailed => record(Event::Starting),
                Step::Stopped => record(Event::Stopped),
            }
            let snapshot = pool.snapshot();
            assert_eq!(snapshot.workers[0].state, case.want, "{}", case.name);
        }
    }
}
