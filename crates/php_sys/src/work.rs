use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use crate::{
    grpc,
    scoreboard::{Event, sb_update},
    start::{Pulled, pull_job_try, pull_job_wait},
    types::Job,
};

pub(crate) enum Work {
    Http(Box<Job>),
    Grpc(Box<grpc::Job>),
}

impl Work {
    pub(crate) fn cancelled(&self) -> bool {
        match self {
            Self::Http(job) => job.ctx.sender.as_ref().is_some_and(|tx| tx.is_closed()),
            Self::Grpc(job) => job.cancelled(),
        }
    }

    pub(crate) fn unavailable(self) {
        match self {
            Self::Http(mut job) => {
                crate::callbacks::send_error_head(&mut job.ctx, 503);
                job.ctx.finish(false);
            }
            Self::Grpc(job) => {
                let _ = job
                    .sender
                    .send(grpc::Reply::error(14, "worker unavailable"));
            }
        }
        sb_update(Event::Shed);
    }
}

// The envelope owns the pending count while waiting to send and while queued.
pub(crate) struct Queued {
    pub(crate) work: Work,
    pub(crate) _pending: PendingGuard,
}

pub(crate) struct PendingGuard(Arc<AtomicUsize>);

impl PendingGuard {
    pub(crate) fn arm(pending: &Arc<AtomicUsize>) -> Self {
        pending.fetch_add(1, Ordering::Relaxed);
        Self(pending.clone())
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Default)]
struct Cycle {
    closed: bool,
    received: bool,
    served: bool,
}

thread_local! {
    static CYCLE: Cell<Cycle> = Cell::new(Cycle::default());
}

fn update(f: impl FnOnce(&mut Cycle)) {
    let mut cycle = CYCLE.get();
    f(&mut cycle);
    CYCLE.set(cycle);
}

pub(crate) fn note_closed() {
    update(|c| c.closed = true);
}
pub(crate) fn note_received() {
    update(|c| c.received = true);
}
pub(crate) fn note_served() {
    update(|c| c.served = true);
}
pub(crate) fn closed_seen() -> bool {
    CYCLE.get().closed
}
pub(crate) fn received_any() -> bool {
    CYCLE.get().received
}
pub(crate) fn served_any() -> bool {
    CYCLE.get().served
}

pub(crate) fn reclaim_current() {
    crate::exchange::reclaim_current();
    grpc::reclaim();
}

pub(crate) fn cycle_reset() {
    reclaim_current();
    CYCLE.set(Cycle::default());
}

pub(crate) struct ReceiveWait {
    timeout: Option<Duration>,
    started: Instant,
}

impl ReceiveWait {
    pub(crate) fn new(timeout_us: i64) -> Self {
        Self {
            timeout: (timeout_us >= 0).then(|| Duration::from_micros(timeout_us as u64)),
            started: Instant::now(),
        }
    }

    pub(crate) fn pull(&self) -> Pulled {
        loop {
            let pulled = match self.timeout {
                Some(t) if t.is_zero() => pull_job_try(),
                Some(t) => {
                    let Some(left) = t.checked_sub(self.started.elapsed()) else {
                        return Pulled::Timeout;
                    };
                    pull_job_wait(Some(left))
                }
                None => pull_job_wait(None),
            };
            if let Pulled::Job(job) = &pulled
                && job.cancelled()
            {
                sb_update(Event::Handled(true));
                continue;
            }
            return pulled;
        }
    }
}
