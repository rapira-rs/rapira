use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering::SeqCst};

use tracing::info;

use crate::plugin::Stopper;
use crate::scoreboard::{Event, sb_update};

pub const STOP_UNHEALTHY: u8 = 1;

/// Why the worker stops: 0 = no stop requested.
pub static STOP_REASON: AtomicU8 = AtomicU8::new(0);

/// The units the current app instance may take: it takes no more once `served` reaches `max`.
#[derive(Default)]
struct QuotaState {
    served: u64,
    max: u64,
    stopper: Stopper,
}

thread_local! {
    static Q: RefCell<QuotaState> = RefCell::new(QuotaState::default());
}

/// Install on the PHP worker thread before the first unit. `max_requests` 0 is no limit.
pub(crate) fn install(max_requests: u64, stopper: Stopper) {
    Q.with_borrow_mut(|q| {
        *q = QuotaState {
            served: 0,
            max: if max_requests == 0 {
                u64::MAX
            } else {
                max_requests
            },
            stopper,
        };
    });
}

/// Drains and stops the plugin once per raise of the reason. fetch_max returns the previous reason, so a repeat is a no-op.
fn stop(reason: u8, stopper: &Stopper) {
    if STOP_REASON.fetch_max(reason, SeqCst) < reason {
        sb_update(Event::Draining);
        stopper.stop();
    }
}

pub(crate) fn tick() {
    Q.with_borrow_mut(|q| {
        q.served += 1;
        if q.served == q.max {
            info!(target: "rapira", "app served {} requests; restarting the app", q.served);
        }
    });
}

/// The current app instance takes no more units.
pub(crate) fn exhausted() -> bool {
    Q.with_borrow(|q| q.served >= q.max)
}

/// A new app instance starts with the whole quota.
pub(crate) fn renew() {
    Q.with_borrow_mut(|q| q.served = 0);
}

pub(crate) fn fire_unhealthy() {
    Q.with_borrow(|q| stop(STOP_UNHEALTHY, &q.stopper));
}
