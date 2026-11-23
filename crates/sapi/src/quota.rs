use std::cell::RefCell;

use tracing::info;

/// How the current app instance ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// The intake closed. The worker stops.
    Stop,
    /// The worker starts a new app instance.
    Recycle,
}

/// The current app instance. It takes no more units when `end` is set.
#[derive(Default)]
struct Instance {
    served: u64,
    max: u64,
    /// The app took a unit.
    received: bool,
    end: Option<End>,
}

thread_local! {
    static INSTANCE: RefCell<Instance> = RefCell::new(Instance::default());
}

/// Installs the quota on the PHP worker thread before the first unit. A `max_requests` of 0 means no limit.
pub(crate) fn install(max_requests: u64) {
    INSTANCE.with_borrow_mut(|i| {
        *i = Instance {
            max: if max_requests == 0 {
                u64::MAX
            } else {
                max_requests
            },
            ..Instance::default()
        };
    });
}

/// Counts a served unit. The app instance ends when the count reaches the quota.
pub(crate) fn tick() {
    INSTANCE.with_borrow_mut(|i| {
        i.served += 1;
        if i.served == i.max {
            info!(target: "rapira", "app served {} requests; restarting the app", i.served);
            i.end.get_or_insert(End::Recycle);
        }
    });
}

pub(crate) fn received() {
    INSTANCE.with_borrow_mut(|i| i.received = true);
}

/// The current app instance takes no more units. A stop replaces a recycle.
pub(crate) fn end(end: End) {
    INSTANCE.with_borrow_mut(|i| {
        if end == End::Stop || i.end.is_none() {
            i.end = Some(end);
        }
    });
}

/// The current app instance takes no more units.
pub(crate) fn ended() -> bool {
    INSTANCE.with_borrow(|i| i.end.is_some())
}

/// How the app instance ended. An instance that took a unit recycles. None is a failed boot.
pub(crate) fn outcome() -> Option<End> {
    INSTANCE.with_borrow(|i| i.end.or(i.received.then_some(End::Recycle)))
}

/// A new app instance starts with the whole quota.
pub(crate) fn renew() {
    INSTANCE.with_borrow_mut(|i| {
        *i = Instance {
            max: i.max,
            ..Instance::default()
        };
    });
}
