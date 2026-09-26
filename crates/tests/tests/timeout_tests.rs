#![cfg(not(target_os = "macos"))]

use rapira_sapi::{Mode, Rapira};
use std::path::Path;
use tests::{captured, drain, fixture, init_log_capture, php_lock_with_ini, req};

/// Pins that receive() disarms the wall timer while parked: a worker parked past the 1s budget still serves instead of fataling and being 503-shed.
#[test]
fn parked_receive_outlives_the_execution_budget() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Dispatcher,
        fixture("dispatcher/echo-loop-worker.php"),
        Some(rapira_http::DISPATCHER_CLASSES),
    )?;
    let h = r.sink();

    let (status, body) = drain(tests::submit(&h, req("/warmup"))?);
    assert_eq!((status, body.as_str()), (200, "method=GET body="));

    std::thread::sleep(std::time::Duration::from_secs(2));
    let (status, body) = drain(tests::submit(&h, req("/first"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "method=GET body="),
        "a worker parked past the budget must still serve"
    );

    drop(h);
    drop(r);
    Ok(())
}

/// Pins that the budget re-armed at unit handout still fires: a spinning unit is killed with its response unsealed and the recycled worker keeps serving.
#[test]
fn rearmed_budget_kills_a_spinning_unit() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Dispatcher,
        fixture("dispatcher/verbs-worker.php"),
        Some(rapira_http::DISPATCHER_CLASSES),
    )?;
    let h = r.sink();

    let (status, body) = drain(tests::submit(&h, req("/"))?);
    assert_eq!((status, body.as_str()), (200, "state=false"));

    let mut rx = tests::submit(&h, req("/?probe=spin"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let resp = tests::drain_resp_deadline(&mut rx, deadline)
        .expect("spinning unit was never killed - the per-unit budget did not re-arm");
    assert!(
        resp.head.is_none() && !resp.ended,
        "a spinning unit must not seal a response (got status {})",
        resp.status()
    );

    let (status, body) = drain(tests::submit(&h, req("/"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must recover after the timeout"
    );

    drop(h);
    drop(r);
    Ok(())
}

/// Pins that the per-job re-arm with reset_signals=0 still delivers SIGRTMIN: a spin on a later job in the same cycle is killed, not left running.
#[test]
fn max_execution_time_fires_on_rearmed_jobs() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("timeout_tests/timeout-worker.php"),
        None,
    )?;
    let h = r.sink();

    let (status, body) = drain(tests::submit(&h, req("/timeout-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "ok"));

    let mut rx = tests::submit(&h, req("/timeout-worker.php?mode=spin"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let resp = tests::drain_resp_deadline(&mut rx, deadline)
        .expect("spinning job was never killed - max_execution_time did not fire");
    assert!(resp.ended, "worker died without sealing a response");
    let body = resp.body_string();
    assert!(
        body.contains("Maximum execution time"),
        "the timeout fatal must reach the body (got: {body:?})"
    );
    assert_eq!(resp.status(), 200);

    let (status, body) = drain(tests::submit(&h, req("/timeout-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "ok"),
        "the worker must recover after a timeout"
    );

    drop(h);
    drop(r);
    Ok(())
}

/// Pins the budget of a unit that is already queued when receive() is called: the spin is killed with the timeout fatal, a unit that fits the budget completes after a unit that used most of its own, and a 0 budget stays untimed after set_time_limit().
#[test]
fn queued_unit_gets_a_fresh_budget() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        first: &'static str,
        queued: &'static str,
        killed: bool,
    }
    let cases = [
        Case {
            name: "queued spin is killed",
            fixture: "timeout_tests/timeout-dispatcher.php",
            first: "/?burn=300",
            queued: "/?spin",
            killed: true,
        },
        Case {
            name: "queued unit gets a full budget",
            fixture: "timeout_tests/timeout-dispatcher.php",
            first: "/?burn=600",
            queued: "/?burn=600",
            killed: false,
        },
        Case {
            name: "queued unit runs untimed after set_time_limit() on a 0 budget",
            fixture: "timeout_tests/timeout-dispatcher-unlimited.php",
            first: "/?limit=1&burn=500",
            queued: "/?burn=1000",
            killed: false,
        },
    ];

    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    init_log_capture();
    for c in cases {
        captured().clear();
        let r = Rapira::start(
            &tests::PHP_PARTS,
            Mode::Dispatcher,
            fixture(c.fixture),
            Some(rapira_http::DISPATCHER_CLASSES),
        )?;
        let h = r.sink();

        let first = tests::submit(&h, req(c.first))?;
        let mut queued = tests::submit(&h, req(c.queued))?;
        assert_eq!(
            drain(first),
            (200, "pending=1".to_owned()),
            "{}: the second unit must be queued before the next receive()",
            c.name
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let resp = tests::drain_resp_deadline(&mut queued, deadline)
            .unwrap_or_else(|| panic!("{}: the queued unit never ended", c.name));
        let fatal = captured()
            .iter()
            .any(|e| e.target == "php" && e.message.contains("Maximum execution time"));
        if c.killed {
            assert!(
                resp.head.is_none() && !resp.ended,
                "{}: a killed unit must not seal a response (got status {})",
                c.name,
                resp.status()
            );
            assert!(fatal, "{}: the timeout fatal must be logged", c.name);
        } else {
            assert_eq!(
                (resp.status(), resp.body_string().as_str()),
                (200, "pending=0"),
                "{}",
                c.name
            );
            assert!(!fatal, "{}: no timeout fatal expected", c.name);
        }

        let (status, body) = drain(tests::submit(&h, req("/"))?);
        assert_eq!(
            (status, body.as_str()),
            (200, "pending=0"),
            "{}: the worker must keep serving",
            c.name
        );

        drop(h);
        drop(r);
    }
    Ok(())
}
