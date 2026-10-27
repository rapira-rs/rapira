use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use tracing::info;

use crate::plugin::Stopper;
use crate::scoreboard::{Event, sb_update};

/// The worker stops because PHP keeps failing to boot.
pub static UNHEALTHY: AtomicBool = AtomicBool::new(false);

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

/// A reload: the current app instance takes no more units.
pub(crate) fn exhaust() {
    Q.with_borrow_mut(|q| q.served = q.max);
}

/// A new app instance starts with the whole quota.
pub(crate) fn renew() {
    Q.with_borrow_mut(|q| q.served = 0);
}

/// Drains and stops the plugin once; a repeat is a no-op.
pub(crate) fn fire_unhealthy() {
    if !UNHEALTHY.swap(true, SeqCst) {
        sb_update(Event::Draining);
        Q.with_borrow(|q| q.stopper.stop());
    }
}
