use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::queue::PoolQueue;
use crate::work::Unit;

#[derive(Default)]
struct StubState {
    settled: AtomicBool,
    cancelled: AtomicBool,
    discarded: AtomicUsize,
    dropped: AtomicUsize,
}

struct StubHeld(Arc<StubState>);

impl Held for StubHeld {
    fn settled(&self) -> bool {
        self.0.settled.load(Ordering::Relaxed)
    }
    fn cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Relaxed)
    }
    fn discard(&mut self) {
        self.0.discarded.fetch_add(1, Ordering::Relaxed);
        self.0.settled.store(true, Ordering::Relaxed);
    }
    fn lost(&mut self) {}
    fn shed(self: Box<Self>) {}
    fn class(&self) -> *mut crate::zend_class_entry {
        null_mut()
    }
}

impl Drop for StubHeld {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

fn test_worker(max_requests: u64) -> (Worker, Arc<Pool>) {
    let (queue, _intake) = PoolQueue::new();
    let pool = Arc::new(Pool::new("test", 1, Arc::clone(&queue)));
    let worker = Worker::new(
        queue.worker_slot(),
        Arc::clone(&pool),
        0,
        Arc::<Workers>::default(),
        Arc::<OpcacheCoordinator>::default(),
        max_requests,
    );
    (worker, pool)
}

fn attach_held(worker: &Worker, settled: bool, cancelled: bool) -> (*mut dyn Held, Arc<StubState>) {
    let state = Arc::new(StubState {
        settled: AtomicBool::new(settled),
        cancelled: AtomicBool::new(cancelled),
        ..StubState::default()
    });
    let ptr = Box::into_raw(Box::new(StubHeld(Arc::clone(&state))) as Box<dyn Held>);
    worker.current_unit.set(Some(ptr));
    worker.current_unit_counted.set(false);
    (ptr, state)
}

#[test]
fn a_failed_boot_exits_only_before_the_pool_serves() {
    struct Case {
        name: &'static str,
        failures: u32,
        limit: u32,
        served: bool,
        exit: bool,
    }
    let cases = [
        Case {
            name: "the next boot can still succeed",
            failures: 2,
            limit: 3,
            served: false,
            exit: false,
        },
        Case {
            name: "the pool never served and this worker reached the limit",
            failures: 3,
            limit: 3,
            served: false,
            exit: true,
        },
        Case {
            name: "another worker served before this worker reached the limit",
            failures: 3,
            limit: 3,
            served: true,
            exit: false,
        },
        Case {
            name: "a pool that served continues to retry",
            failures: 4,
            limit: 3,
            served: true,
            exit: false,
        },
        Case {
            name: "zero disables the limit even if the pool never served",
            failures: 5,
            limit: 0,
            served: false,
            exit: false,
        },
    ];
    for case in cases {
        assert_eq!(
            give_up_boot(case.failures, case.limit, case.served),
            case.exit,
            "{}",
            case.name
        );
    }
}

fn text(boot: &OpcacheBoot) -> String {
    match boot {
        OpcacheBoot::Run => "run".to_owned(),
        OpcacheBoot::LeaveAtFirstPull => "leave at the first pull".to_owned(),
        OpcacheBoot::Wait { seen } => format!("wait until last_restart_time is not {seen}"),
    }
}

#[test]
fn opcache_boot_follows_the_opcache_state() {
    struct Case {
        name: &'static str,
        state: c_int,
        last_restart: zend_long,
        previous_end: Option<zend_long>,
        want: OpcacheBoot,
    }
    let cases = [
        Case {
            name: "no OPcache",
            state: -1,
            last_restart: 0,
            previous_end: None,
            want: OpcacheBoot::Run,
        },
        Case {
            name: "OPcache on",
            state: 1,
            last_restart: 7,
            previous_end: Some(7),
            want: OpcacheBoot::Run,
        },
        Case {
            name: "on, and a restart is pending",
            state: 1 | 2,
            last_restart: 7,
            previous_end: Some(7),
            want: OpcacheBoot::LeaveAtFirstPull,
        },
        Case {
            name: "off, and a restart is pending",
            state: 2,
            last_restart: 7,
            previous_end: Some(7),
            want: OpcacheBoot::Wait { seen: 7 },
        },
        Case {
            name: "off, a restart is pending, first app instance of a process",
            state: 2,
            last_restart: 7,
            previous_end: None,
            want: OpcacheBoot::Wait { seen: 7 },
        },
        Case {
            name: "off, and a restart runs now",
            state: 4,
            last_restart: 7,
            previous_end: Some(7),
            want: OpcacheBoot::Wait { seen: 7 },
        },
        Case {
            name: "off, and a restart ran after the previous app instance",
            state: 0,
            last_restart: 8,
            previous_end: Some(7),
            want: OpcacheBoot::Wait { seen: 7 },
        },
        Case {
            name: "off, and no restart ran after the previous app instance",
            state: 0,
            last_restart: 7,
            previous_end: Some(7),
            want: OpcacheBoot::Run,
        },
        Case {
            name: "off with no sign of a restart, first app instance of a process",
            state: 0,
            last_restart: 7,
            previous_end: None,
            want: OpcacheBoot::Run,
        },
    ];
    for case in cases {
        let got = opcache_boot(case.state, case.last_restart, case.previous_end);
        assert_eq!(text(&got), text(&case.want), "{}", case.name);
    }
}

#[test]
fn dispatcher_unit_accounting_counts_once_and_reclaims_unsettled() {
    struct Case {
        name: &'static str,
        settled: bool,
        cancelled: bool,
        forget_first: bool,
        reclaim_instead_of_forget: bool,
        want_can_receive: bool,
        want_discarded: usize,
        want_served: u64,
        want_errors: u64,
    }
    let cases = [
        Case {
            name: "settled unit counts once across repeat settle and later forget_held",
            settled: true,
            cancelled: false,
            forget_first: false,
            reclaim_instead_of_forget: false,
            want_can_receive: true,
            want_discarded: 0,
            want_served: 1,
            want_errors: 0,
        },
        Case {
            name: "early forget_held counts once and leaves slot empty for next receive",
            settled: true,
            cancelled: false,
            forget_first: true,
            reclaim_instead_of_forget: false,
            want_can_receive: true,
            want_discarded: 0,
            want_served: 1,
            want_errors: 0,
        },
        Case {
            name: "unsettled live unit blocks next receive without counting quota",
            settled: false,
            cancelled: false,
            forget_first: false,
            reclaim_instead_of_forget: false,
            want_can_receive: false,
            want_discarded: 0,
            want_served: 0,
            want_errors: 0,
        },
        Case {
            name: "unsettled cancelled unit is discarded and counted once",
            settled: false,
            cancelled: true,
            forget_first: false,
            reclaim_instead_of_forget: false,
            want_can_receive: true,
            want_discarded: 1,
            want_served: 1,
            want_errors: 0,
        },
        Case {
            name: "leaked unsettled unit is reclaimed and recorded as errored at cycle end",
            settled: false,
            cancelled: false,
            forget_first: false,
            reclaim_instead_of_forget: true,
            want_can_receive: false,
            want_discarded: 0,
            want_served: 1,
            want_errors: 1,
        },
    ];

    for case in cases {
        let (worker, pool) = test_worker(1);
        let (ptr, state) = attach_held(&worker, case.settled, case.cancelled);

        if case.forget_first {
            worker.forget_held(ptr.cast::<()>());
            drop(unsafe { Box::from_raw(ptr) });
        }

        let can_receive = worker.settle_previous_unit();
        assert_eq!(can_receive, case.want_can_receive, "{}", case.name);
        // Calling settle_previous_unit again (as after an empty tryReceive) must never double-count.
        assert_eq!(
            worker.settle_previous_unit(),
            case.want_can_receive,
            "{}",
            case.name
        );

        if case.reclaim_instead_of_forget {
            worker.reclaim_current();
            assert!(!worker.has_active_unit(), "{}", case.name);
        } else if !case.forget_first {
            // If still unsettled, free directly without ticking via forget_held so served stays 0.
            if case.want_can_receive {
                worker.forget_held(ptr.cast::<()>());
            } else {
                worker.current_unit.set(None);
            }
            drop(unsafe { Box::from_raw(ptr) });
        }

        assert_eq!(
            state.discarded.load(Ordering::Relaxed),
            case.want_discarded,
            "{}",
            case.name
        );
        assert_eq!(state.dropped.load(Ordering::Relaxed), 1, "{}", case.name);
        assert_eq!(worker.served.get(), case.want_served, "{}", case.name);
        assert_eq!(worker.ended(), case.want_served >= 1, "{}", case.name);
        assert_eq!(
            pool.snapshot().workers[0].errors,
            case.want_errors,
            "{}",
            case.name
        );
    }
}

#[test]
fn stale_forget_held_and_host_reset_hook_flags_keep_isolation() {
    struct Case {
        name: &'static str,
        host_reset: bool,
        want_scheduled: Option<c_int>,
    }
    let cases = [
        Case {
            name: "host opcache_reset suppresses restart hook",
            host_reset: true,
            want_scheduled: None,
        },
        Case {
            name: "script restart hook records reason",
            host_reset: false,
            want_scheduled: Some(0),
        },
    ];
    for case in cases {
        let (worker, _pool) = test_worker(2);
        let (p1, _s1) = attach_held(&worker, true, false);
        let (p2, _s2) = attach_held(&worker, false, false);
        // Stale forget_held(p1) must not disturb p2 or tick quota.
        worker.forget_held(p1.cast::<()>());
        assert!(worker.has_active_unit(), "{}", case.name);
        assert_eq!(worker.served.get(), 0, "{}", case.name);

        worker.host_reset.set(case.host_reset);
        worker.on_opcache_restart_scheduled(0);
        assert_eq!(
            worker.opcache_scheduled.get(),
            case.want_scheduled,
            "{}",
            case.name
        );

        worker.current_unit.set(None);
        drop(unsafe { Box::from_raw(p1) });
        drop(unsafe { Box::from_raw(p2) });
    }
}

#[tokio::test(flavor = "current_thread")]
async fn timed_pull_stops_when_cancelled_unit_consumes_deadline() {
    struct Case {
        name: &'static str,
        wait: Wait,
        queue_cancelled_first: bool,
        want_unit: bool,
        want_live_left_in_queue: bool,
    }
    let cases = [
        Case {
            name: "expired Wait::For stops after skipping cancelled unit instead of zero-polling live unit",
            wait: Wait::For(Duration::ZERO),
            queue_cancelled_first: true,
            want_unit: false,
            want_live_left_in_queue: true,
        },
        Case {
            name: "unexpired Wait::For skips cancelled unit and returns live unit",
            wait: Wait::For(Duration::from_secs(1)),
            queue_cancelled_first: true,
            want_unit: true,
            want_live_left_in_queue: false,
        },
        Case {
            name: "Wait::Never drains cancelled unit and returns live unit",
            wait: Wait::Never,
            queue_cancelled_first: true,
            want_unit: true,
            want_live_left_in_queue: false,
        },
        Case {
            name: "Wait::Forever skips cancelled unit and returns live unit",
            wait: Wait::Forever,
            queue_cancelled_first: true,
            want_unit: true,
            want_live_left_in_queue: false,
        },
        Case {
            name: "Wait::For(0) returns an immediately queued live unit",
            wait: Wait::For(Duration::ZERO),
            queue_cancelled_first: false,
            want_unit: true,
            want_live_left_in_queue: false,
        },
    ];

    for case in cases {
        let (queue, intake) = PoolQueue::new();
        let pool = Arc::new(Pool::new("test", 1, Arc::clone(&queue)));
        let worker = Worker::new(
            queue.worker_slot(),
            Arc::clone(&pool),
            0,
            Arc::<Workers>::default(),
            Arc::<OpcacheCoordinator>::default(),
            0,
        );
        if case.queue_cancelled_first {
            let cancelled = Arc::new(StubState {
                cancelled: AtomicBool::new(true),
                ..StubState::default()
            });
            intake
                .submit(Unit::Dispatcher(Box::new(StubHeld(cancelled))), 0)
                .await
                .unwrap();
        }
        let live = Arc::new(StubState::default());
        intake
            .submit(Unit::Dispatcher(Box::new(StubHeld(live))), 0)
            .await
            .unwrap();

        let got = worker.next_pull(case.wait);
        assert_eq!(
            matches!(got, Pull::Unit(_)),
            case.want_unit,
            "{}",
            case.name
        );
        let remaining = worker.next_pull(Wait::Never);
        assert_eq!(
            matches!(remaining, Pull::Unit(_)),
            case.want_live_left_in_queue,
            "{}",
            case.name
        );
    }
}
