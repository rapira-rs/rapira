use super::*;
use crate::work::Held;

struct Probe;

impl Held for Probe {
    fn settled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> bool {
        false
    }
    fn discard(&mut self) {}
    fn lost(&mut self) {}
    fn shed(self: Box<Self>) {}
    fn class(&self) -> *mut crate::zend_class_entry {
        std::ptr::null_mut()
    }
}

fn probe() -> Unit {
    Unit::Dispatcher(Box::new(Probe))
}

#[tokio::test(flavor = "current_thread")]
async fn submitters_share_the_body_budget_of_the_pool() {
    struct Case {
        name: &'static str,
        queued_bytes: usize,
        next_bytes: usize,
        result: Result<(), Refused>,
        pending: u64,
        refused: u64,
    }
    let cases = [
        Case {
            name: "the next body fits the remaining byte",
            queued_bytes: BODY_BUDGET - 1,
            next_bytes: 1,
            result: Ok(()),
            pending: 2,
            refused: 0,
        },
        Case {
            name: "a large first body holds the whole budget",
            queued_bytes: BODY_BUDGET + 1,
            next_bytes: 1,
            result: Err(Refused::OverBudget),
            pending: 1,
            refused: 1,
        },
    ];
    for case in cases {
        let (queue, intake) = PoolQueue::new();
        intake.submit(probe(), case.queued_bytes).await.unwrap();
        let other = intake.clone();
        let result = other.submit(probe(), case.next_bytes).await;
        assert_eq!(result, case.result, "{}", case.name);
        let stats = queue.stats();
        assert_eq!(
            (stats.pending, stats.refused, stats.failed_on_full_queue),
            (case.pending, case.refused, 0),
            "{}",
            case.name
        );
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_full_intake_counts_each_unit_that_never_enters() {
    enum During {
        Nothing,
        ClientLeaves,
        RoomFrees,
    }
    struct Case {
        name: &'static str,
        during: During,
        result: Option<Result<(), Refused>>,
        refused: u64,
        failed_on_full_queue: u64,
    }
    const ACTION_DELAY: Duration = Duration::from_secs(1);
    let cases = [
        Case {
            name: "shed after the wait",
            during: During::Nothing,
            result: Some(Err(Refused::Saturated)),
            refused: 1,
            failed_on_full_queue: 1,
        },
        Case {
            name: "the client leaves during the wait",
            during: During::ClientLeaves,
            result: None,
            refused: 0,
            failed_on_full_queue: 1,
        },
        Case {
            name: "room frees during the wait",
            during: During::RoomFrees,
            result: Some(Ok(())),
            refused: 0,
            failed_on_full_queue: 0,
        },
    ];
    for case in cases {
        let (queue, intake) = PoolQueue::new();
        let slot = queue.worker_slot();
        for _ in 0..INTAKE_SLOTS {
            intake.submit(probe(), 0).await.unwrap();
        }
        let submit = intake.submit(probe(), 0);
        let result = match case.during {
            During::Nothing => Some(submit.await),
            During::ClientLeaves => tokio::time::timeout(ACTION_DELAY, submit).await.ok(),
            During::RoomFrees => {
                let (result, ()) = tokio::join!(submit, async {
                    tokio::time::sleep(ACTION_DELAY).await;
                    assert!(matches!(slot.recv(Duration::ZERO), Pull::Unit(_)));
                });
                Some(result)
            }
        };
        assert_eq!(result, case.result, "{}", case.name);
        let stats = queue.stats();
        assert_eq!(
            (stats.refused, stats.failed_on_full_queue, stats.pending),
            (case.refused, case.failed_on_full_queue, INTAKE_SLOTS as u64),
            "{}",
            case.name
        );
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn saturated_or_cancelled_submit_returns_body_budget() {
    struct Case {
        name: &'static str,
        cancel_early: bool,
    }
    let cases = [
        Case {
            name: "timed out submit restores full body budget",
            cancel_early: false,
        },
        Case {
            name: "cancelled submit restores full body budget",
            cancel_early: true,
        },
    ];
    for case in cases {
        let (queue, intake) = PoolQueue::new();
        let slot = queue.worker_slot();
        for _ in 0..INTAKE_SLOTS {
            intake.submit(probe(), 0).await.unwrap();
        }
        let submit = intake.submit(probe(), BODY_BUDGET);
        if case.cancel_early {
            assert!(
                tokio::time::timeout(Duration::from_secs(1), submit)
                    .await
                    .is_err(),
                "{}",
                case.name
            );
        } else {
            assert_eq!(submit.await, Err(Refused::Saturated), "{}", case.name);
        }
        assert!(matches!(slot.recv(Duration::ZERO), Pull::Unit(_)));
        assert_eq!(
            intake.submit(probe(), BODY_BUDGET).await,
            Ok(()),
            "{}",
            case.name
        );
    }
}

#[test]
fn failing_budget_reservation_does_not_block_fitting_request() {
    struct Case {
        name: &'static str,
        avail: u32,
        too_big: u32,
        fits: u32,
    }
    let cases = [
        Case {
            name: "2-byte over-budget attempt never blocks 1-byte reservation",
            avail: 1,
            too_big: 2,
            fits: 1,
        },
        Case {
            name: "1 MiB over-budget attempt never blocks 4 KiB reservation",
            avail: 4096,
            too_big: 1 << 20,
            fits: 4096,
        },
    ];
    for case in cases {
        let limits = Arc::new(Limits::new());
        assert!(limits.try_acquire_budget((BODY_BUDGET as u32) - case.avail));
        let stop = Arc::new(AtomicBool::new(false));
        let l_bg = Arc::clone(&limits);
        let s_bg = Arc::clone(&stop);
        let too_big = case.too_big;
        let contender = thread::spawn(move || {
            while !s_bg.load(Ordering::Relaxed) {
                assert!(!l_bg.try_acquire_budget(too_big));
            }
        });
        let mut false_refusals = 0usize;
        for _ in 0..100_000 {
            if limits.try_acquire_budget(case.fits) {
                limits.release_budget(case.fits);
            } else {
                false_refusals += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        contender.join().unwrap();
        assert_eq!(false_refusals, 0, "{}", case.name);
    }
}

#[test]
fn reload_pull_hands_off_queued_unit_to_parked_worker() {
    struct Case {
        name: &'static str,
        park_reloading_worker: bool,
    }
    let cases = [
        Case {
            name: "immediate take returning reload wakes parked peer for queued unit",
            park_reloading_worker: false,
        },
        Case {
            name: "parked worker woken into reload hands off queued unit to parked peer",
            park_reloading_worker: true,
        },
    ];
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for case in cases {
        let (queue, intake) = PoolQueue::new();
        let (got_tx, got_rx) = bounded(1);

        let wait_for_waiters = |want: usize| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while queue.waiter_count.0.load(Ordering::Relaxed) < want {
                assert!(
                    Instant::now() < deadline,
                    "{}: expected {want} parked workers",
                    case.name
                );
                thread::yield_now();
            }
        };

        if case.park_reloading_worker {
            let w1 = queue.worker_slot();
            let w2 = queue.worker_slot();
            // Park w2 first (at the bottom of the LIFO stack) on generation 1 so submit wakes w1 first.
            w2.seen_reload.set(1);
            let t2 = thread::spawn(move || {
                let pull = w2.recv(Duration::from_secs(2));
                let _ = got_tx.send(matches!(pull, Pull::Unit(_)));
            });
            wait_for_waiters(1);

            let (w1_tx, w1_rx) = bounded(1);
            let t1 = thread::spawn(move || {
                let pull = w1.recv(Duration::from_secs(2));
                let _ = w1_tx.send(matches!(pull, Pull::Reload));
            });
            wait_for_waiters(2);

            // Publish reload generation 1 without waking directly; submit wakes LIFO top (w1), which hands off the unit to w2.
            queue.reload_gen.store(1, Ordering::SeqCst);
            rt.block_on(intake.submit(probe(), 0)).unwrap();
            assert_eq!(
                w1_rx.recv_timeout(Duration::from_millis(500)).ok(),
                Some(true),
                "{}: w1 should pull Reload",
                case.name
            );
            t1.join().unwrap();

            let w2_got_unit = got_rx.recv_timeout(Duration::from_millis(500)).ok();
            drop(intake);
            t2.join().unwrap();
            assert_eq!(w2_got_unit, Some(true), "{}", case.name);
        } else {
            let w1 = queue.worker_slot();
            queue.reload();
            let w2 = queue.worker_slot();
            let t2 = thread::spawn(move || {
                let pull = w2.recv(Duration::from_secs(2));
                let _ = got_tx.send(matches!(pull, Pull::Unit(_)));
            });
            wait_for_waiters(1);

            rt.block_on(intake.submit(probe(), 0)).unwrap();
            assert!(
                matches!(w1.recv(Duration::ZERO), Pull::Reload),
                "{}: w1 should pull Reload",
                case.name
            );

            let w2_got_unit = got_rx.recv_timeout(Duration::from_millis(500)).ok();
            drop(intake);
            t2.join().unwrap();
            assert_eq!(w2_got_unit, Some(true), "{}", case.name);
        }
    }
}

#[test]
fn pull_to_shed_respects_readiness_reload_and_close() {
    enum Trigger {
        ReadinessLost,
        Reload,
        IntakeDropWhileReady,
    }
    struct Case {
        name: &'static str,
        initially_ready: bool,
        trigger: Trigger,
        want_unit: bool,
        want_reload: bool,
        want_closed: bool,
    }
    let cases = [
        Case {
            name: "shedder wakes and sheds queued unit when last ready worker leaves",
            initially_ready: true,
            trigger: Trigger::ReadinessLost,
            want_unit: true,
            want_reload: false,
            want_closed: false,
        },
        Case {
            name: "reload wakes parked shedder immediately while pool is ready",
            initially_ready: true,
            trigger: Trigger::Reload,
            want_unit: false,
            want_reload: true,
            want_closed: false,
        },
        Case {
            name: "dropping intake wakes parked shedder with Closed even while pool is ready",
            initially_ready: true,
            trigger: Trigger::IntakeDropWhileReady,
            want_unit: false,
            want_reload: false,
            want_closed: true,
        },
    ];
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for case in cases {
        let (queue, intake) = PoolQueue::new();
        queue.sync_ready(|| case.initially_ready);
        rt.block_on(intake.submit(probe(), 0)).unwrap();
        let slot = queue.worker_slot();
        let (tx, rx) = bounded(1);

        let shedder = thread::spawn(move || {
            let pull = slot.pull_to_shed(Instant::now() + Duration::from_secs(2));
            let _ = tx.send((
                matches!(pull, Pull::Unit(_)),
                matches!(pull, Pull::Reload),
                matches!(pull, Pull::Closed),
            ));
        });

        let spin_deadline = Instant::now() + Duration::from_secs(2);
        while !queue.has_waiters() {
            assert!(
                Instant::now() < spin_deadline,
                "{}: shedder did not park while pool was ready",
                case.name
            );
            thread::yield_now();
        }

        match case.trigger {
            Trigger::ReadinessLost => queue.sync_ready(|| false),
            Trigger::Reload => queue.reload(),
            Trigger::IntakeDropWhileReady => drop(intake),
        }

        let got = rx.recv_timeout(Duration::from_millis(500)).unwrap();
        shedder.join().unwrap();
        assert_eq!(
            got,
            (case.want_unit, case.want_reload, case.want_closed),
            "{}",
            case.name
        );
    }
}
