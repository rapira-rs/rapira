use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering::SeqCst};

use tracing::info;

use crate::plugin::Stopper;
use crate::scoreboard::{Event, sb_update};

// The value order is the priority: a higher reason replaces a lower one.
pub const STOP_QUOTA: u8 = 1;
pub const STOP_UNHEALTHY: u8 = 2;

/// Why the worker stops: 0 = no stop requested. The higher reason wins, so unhealthy replaces a pending quota stop.
pub static STOP_REASON: AtomicU8 = AtomicU8::new(0);

#[derive(Default)]
struct QuotaState {
    served: u64,
    max: u64,
    stopper: Stopper,
}

thread_local! {
    static Q: RefCell<QuotaState> = RefCell::new(QuotaState::default());
}

/// Install on the PHP worker thread before the first job.
pub(crate) fn install(max_requests: u64, stopper: Stopper) {
    Q.with_borrow_mut(|q| {
        *q = QuotaState {
            served: 0,
            max: max_requests,
            stopper,
        };
    });
}

/// Drains and stops the plugin once per raise of the reason. fetch_max returns the previous reason, so a repeat or a lower reason is a no-op.
fn stop(reason: u8, stopper: &Stopper) {
    if STOP_REASON.fetch_max(reason, SeqCst) < reason {
        sb_update(Event::Draining);
        stopper.stop();
    }
}

pub(crate) fn tick() {
    Q.with_borrow_mut(|q| {
        if q.max == 0 {
            return;
        }
        q.served += 1;
        if q.served == q.max {
            info!(target: "rapira", "worker served {} requests; recycling", q.served);
            stop(STOP_QUOTA, &q.stopper);
        }
    });
}

pub(crate) fn fire_unhealthy() {
    Q.with_borrow(|q| stop(STOP_UNHEALTHY, &q.stopper));
}
