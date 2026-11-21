use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use tracing::info;

use crate::plugin::Stopper;
use crate::scoreboard::{Event, sb_update};

/// The worker stops because PHP failed to boot `max_boot_failures` times in sequence.
static UNHEALTHY: AtomicBool = AtomicBool::new(false);

/// The units that the current app instance may take. It takes no more units when `served` reaches `max`.
#[derive(Default)]
struct QuotaState {
    served: u64,
    max: u64,
    stopper: Stopper,
}

thread_local! {
    static Q: RefCell<QuotaState> = RefCell::new(QuotaState::default());
}

/// Installs the quota on the PHP worker thread before the first unit. A `max_requests` of 0 means no limit.
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

/// On a reload, the current app instance takes no more units.
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

pub(crate) fn unhealthy() -> bool {
    UNHEALTHY.load(SeqCst)
}
