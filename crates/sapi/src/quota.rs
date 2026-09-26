use std::cell::RefCell;

use tracing::info;

use crate::scoreboard::{Event, sb_update};

#[derive(Default)]
pub struct WorkerHooks {
    /// 0 = unlimited; jitter already applied by the caller.
    pub max_requests: u64,
    pub on_quota: Option<Box<dyn FnOnce() + Send>>,
    pub on_unhealthy: Option<Box<dyn FnOnce() + Send>>,
    /// None: report into a private single-slot board.
    pub slot: Option<&'static rapira_scoreboard::SharedSlot>,
}

#[derive(Default)]
struct QuotaState {
    served: u64,
    max: u64,
    on_quota: Option<Box<dyn FnOnce() + Send>>,
    on_unhealthy: Option<Box<dyn FnOnce() + Send>>,
}

thread_local! {
    static Q: RefCell<QuotaState> = RefCell::new(QuotaState::default());
}

/// Install on the PHP worker thread before the first job.
pub(crate) fn install(
    max_requests: u64,
    on_quota: Option<Box<dyn FnOnce() + Send>>,
    on_unhealthy: Option<Box<dyn FnOnce() + Send>>,
) {
    Q.with_borrow_mut(|q| {
        *q = QuotaState {
            served: 0,
            max: max_requests,
            on_quota,
            on_unhealthy,
        };
    });
}

pub(crate) fn tick() {
    Q.with_borrow_mut(|q| {
        if q.max == 0 {
            return;
        }
        q.served += 1;
        if q.served == q.max
            && let Some(f) = q.on_quota.take()
        {
            info!(target: "rapira", "worker served {} requests; recycling", q.served);
            sb_update(Event::Draining);
            f();
        }
    });
}

pub(crate) fn fire_unhealthy() {
    Q.with_borrow_mut(|q| {
        if let Some(f) = q.on_unhealthy.take() {
            sb_update(Event::Draining);
            f();
        }
    });
}
