use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Acquire};
use std::time::Instant;

pub const SLOT_STARTING: u32 = 1;
pub const SLOT_IDLE: u32 = 2;
pub const SLOT_ACTIVE: u32 = 3;
pub const SLOT_DRAINING: u32 = 4;

pub type Board = Arc<[Arc<Pool>]>;

pub struct Pool {
    pub name: &'static str,
    pub workers: Box<[Worker]>,
    pub pending: AtomicU64,
    pub failed_on_full_queue: AtomicU64,
    /// Units refused before a worker could take them.
    pub refused: AtomicU64,
    pub served: AtomicBool,
    /// Active workers whose interrupted unit is still running after the watchdog grace period.
    pub blocked: AtomicUsize,
}

impl Pool {
    pub fn new(name: &'static str, count: usize) -> Self {
        Self {
            name,
            workers: (0..count).map(|_| Worker::default()).collect(),
            pending: AtomicU64::new(0),
            failed_on_full_queue: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            served: AtomicBool::new(false),
            blocked: AtomicUsize::new(0),
        }
    }

    pub fn ready(&self) -> bool {
        self.workers.iter().any(Worker::serving)
    }
}

pub struct Worker {
    pub state: AtomicU32,
    pub handled: AtomicU64,
    pub errors: AtomicU64,
    pub recycles: AtomicU64,
    pub last_activity_ms: AtomicU64,
}

impl Default for Worker {
    fn default() -> Self {
        Self {
            state: AtomicU32::new(SLOT_STARTING),
            handled: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            recycles: AtomicU64::new(0),
            last_activity_ms: AtomicU64::new(0),
        }
    }
}

impl Worker {
    pub fn serving(&self) -> bool {
        matches!(self.state.load(Acquire), SLOT_IDLE | SLOT_ACTIVE)
    }
}

/// Monotonic milliseconds from the first call in this process.
pub fn now_millis() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn a_pool_is_ready_when_any_worker_can_serve() {
        struct Case {
            name: &'static str,
            states: [u32; 2],
            ready: bool,
        }
        let cases = [
            Case {
                name: "both starting",
                states: [SLOT_STARTING, SLOT_STARTING],
                ready: false,
            },
            Case {
                name: "one idle",
                states: [SLOT_STARTING, SLOT_IDLE],
                ready: true,
            },
            Case {
                name: "one active",
                states: [SLOT_ACTIVE, SLOT_DRAINING],
                ready: true,
            },
            Case {
                name: "last worker draining",
                states: [SLOT_DRAINING, SLOT_STARTING],
                ready: false,
            },
        ];
        for case in cases {
            let pool = Pool::new("http", 2);
            for (worker, state) in pool.workers.iter().zip(case.states) {
                worker.state.store(state, Relaxed);
            }
            assert_eq!(pool.ready(), case.ready, "{}", case.name);
        }
    }
}
