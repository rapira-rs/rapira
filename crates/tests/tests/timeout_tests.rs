#![cfg(not(target_os = "macos"))]

use php_sys::{Mode, Rapira};
use std::path::Path;
use tests::{drain, fixture, php_lock_with_ini, req};

/// Pins that receive() disarms the wall timer while parked: a worker parked past the 1s budget still serves instead of fataling and being 503-shed.
#[test]
fn parked_receive_outlives_the_execution_budget() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/echo-loop-worker.php")))?;
    let h = r.handle();

    let (status, body) =
        drain(h.handle_blocking(req("/warmup", "dispatcher/echo-loop-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "method=GET body="));

    std::thread::sleep(std::time::Duration::from_secs(2));
    let (status, body) =
        drain(h.handle_blocking(req("/first", "dispatcher/echo-loop-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "method=GET body="),
        "a worker parked past the budget must still serve"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// Pins that the budget re-armed at unit handout still fires: a spinning unit is killed with its response unsealed and the recycled worker keeps serving.
#[test]
fn rearmed_budget_kills_a_spinning_unit() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle();

    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/verbs-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "state=false"));

    let mut rx = h.handle_blocking(req("/?probe=spin", "dispatcher/verbs-worker.php"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let resp = tests::drain_resp_deadline(&mut rx, deadline)
        .expect("spinning unit was never killed - the per-unit budget did not re-arm");
    assert!(
        resp.head.is_none() && !resp.ended,
        "a spinning unit must not seal a response (got status {})",
        resp.status()
    );

    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/verbs-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must recover after the timeout"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// Pins that the per-job re-arm with reset_signals=0 still delivers SIGRTMIN: a spin on a later job in the same cycle is killed, not left running.
#[test]
fn max_execution_time_fires_on_rearmed_jobs() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Worker(fixture("timeout_tests/timeout-worker.php")))?;
    let h = r.handle();

    let (status, body) = drain(h.handle_blocking(req(
        "/timeout-worker.php",
        "timeout_tests/timeout-worker.php",
    ))?);
    assert_eq!((status, body.as_str()), (200, "ok"));

    let mut rx = h.handle_blocking(req(
        "/timeout-worker.php?mode=spin",
        "timeout_tests/timeout-worker.php",
    ))?;
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

    let (status, body) = drain(h.handle_blocking(req(
        "/timeout-worker.php",
        "timeout_tests/timeout-worker.php",
    ))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "ok"),
        "the worker must recover after a timeout"
    );

    drop(h);
    r.shutdown();
    Ok(())
}
