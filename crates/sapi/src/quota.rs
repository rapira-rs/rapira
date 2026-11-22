use std::cell::RefCell;

use tracing::info;

/// The units that the current app instance may take. It takes no more units when `served` reaches `max`.
#[derive(Default)]
struct QuotaState {
    served: u64,
    max: u64,
}

thread_local! {
    static Q: RefCell<QuotaState> = RefCell::new(QuotaState::default());
}

/// Installs the quota on the PHP worker thread before the first unit. A `max_requests` of 0 means no limit.
pub(crate) fn install(max_requests: u64) {
    Q.with_borrow_mut(|q| {
        *q = QuotaState {
            served: 0,
            max: if max_requests == 0 {
                u64::MAX
            } else {
                max_requests
            },
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
