use php_sys::{Mode, Rapira};
use tests::{
    captured, drain, drain_resp, fixture, init_log_capture, php_lock, req, wait_app_record,
};

/// Superglobals are rebuilt per job over the resident loop: query state must not leak.
#[test]
fn worker_serves_with_per_job_superglobals() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/hello-worker.php")))?;
    let h = r.handle();

    let resp = drain_resp(tests::submit(
        &h,
        req("/?q=zap", "worker/hello-worker.php"),
    )?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "hello:GET:zap");
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("text/plain;charset=UTF-8"),
        "header() must reach the head"
    );

    let resp = drain_resp(tests::submit(&h, req("/", "worker/hello-worker.php"))?);
    assert_eq!(
        resp.body_string(),
        "hello:GET:-",
        "query state must not leak"
    );

    drop(h);
    drop(r);
    Ok(())
}

/// Closing the intake makes handle_request() return false: the post-loop code runs exactly once.
#[test]
fn drain_returns_false_and_the_script_completes() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("worker/drain-worker.php")))?;
    let h = r.handle();
    for want in ["n=1", "n=2"] {
        let resp = drain_resp(tests::submit(&h, req("/", "worker/drain-worker.php"))?);
        assert_eq!(resp.body_string(), want, "resident state must accumulate");
    }
    drop(h);
    drop(r);

    let exited = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "loop-exited served=2")
        .count();
    assert_eq!(exited, 1, "the post-loop code must run exactly once");
    Ok(())
}

/// Classic mode: the gate throws `NotInWorkerModeError`, and ZPP rejects a non-callable ahead of it.
#[test]
fn handle_request_outside_worker_mode_throws() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(tests::submit(&h, req("/", "worker/gate-classic.php"))?);
    drop(h);
    drop(r);

    assert_eq!(status, 200, "every throw must be caught (body: {body:?})");
    for line in [
        "class: Rapira\\Exception\\NotInWorkerModeError",
        "rapira: yes",
        "type-error",
        "done",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}

/// Dispatcher mode: the gate refuses before the shared intake is touched, so no unit is stolen.
#[test]
fn handle_request_in_dispatcher_mode_throws() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture(
        "worker/gate-dispatcher-worker.php",
    )))?;
    let h = r.handle();
    let resp = drain_resp(tests::submit(
        &h,
        req("/", "worker/gate-dispatcher-worker.php"),
    )?);
    assert_eq!(
        resp.body_string(),
        "ok",
        "the unit must survive the refusal"
    );
    drop(h);
    drop(r);

    let gated = captured()
        .iter()
        .filter(|c| {
            c.target == "app" && c.message == "gate Rapira\\Exception\\NotInWorkerModeError"
        })
        .count();
    assert_eq!(gated, 1);
    let finish_gated = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "finish-gate")
        .count();
    assert_eq!(
        finish_gated, 1,
        "rapira_finish_request() must refuse dispatcher mode"
    );
    Ok(())
}

/// exit() inside a handler ships that response and leaves the resident loop and its state alive.
#[test]
fn exit_in_a_handler_survives_the_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/exit-worker.php")))?;
    let h = r.handle();

    let resp = drain_resp(tests::submit(&h, req("/?die=1", "worker/exit-worker.php"))?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "n=1", "exit must still ship the body");
    let resp = drain_resp(tests::submit(&h, req("/", "worker/exit-worker.php"))?);
    assert_eq!(resp.body_string(), "n=2", "the loop and its state survive");

    drop(h);
    drop(r);
    Ok(())
}

/// A self-stopping loop classifies Recycle: the next job re-bootstraps and is still served.
#[test]
fn self_stopping_loop_recycles_and_serves_again() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("worker/one-turn-worker.php")))?;
    let h = r.handle();
    for _ in 0..2 {
        let resp = drain_resp(tests::submit(&h, req("/", "worker/one-turn-worker.php"))?);
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.body_string(), "once");
    }
    drop(h);
    drop(r);

    let turns = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "one-turn-done")
        .count();
    assert_eq!(turns, 3, "each bootstrap must run the script to completion");
    Ok(())
}

/// A bootstrap that never calls handle_request() sheds 503 instead of hanging or answering 200.
#[test]
fn never_looping_script_sheds_503() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/never-loop-worker.php")))?;
    let h = r.handle();
    let mut rx = tests::submit(&h, req("/", "worker/never-loop-worker.php"))?;
    let resp = tests::drain_resp_deadline(
        &mut rx,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .expect("the shed 503 never arrived");
    assert_eq!(resp.status(), 503, "a never-serving bootstrap must shed");
    drop(h);
    drop(r);
    Ok(())
}

/// Bootstrap $_ENV survives late compilation: php_auto_globals_create_env dtors the array before checking variables_order.
#[test]
fn bootstrap_env_survives_late_compilation() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/env-worker.php")))?;
    let h = r.handle();
    for job in 0..2 {
        let resp = drain_resp(tests::submit(&h, req("/", "worker/env-worker.php"))?);
        assert_eq!(resp.body_string(), "set-at-boot", "job {job}");
    }
    drop(h);
    drop(r);
    Ok(())
}

/// `Location:` on a POST answers 303: sapi_activate resets proto_num, so a missing re-apply degrades it to 302.
#[test]
fn post_location_redirects_303_in_worker_mode() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/location-worker.php")))?;
    let h = r.handle();
    let mut rq = req("/", "worker/location-worker.php");
    rq.method = "POST".into();
    let resp = drain_resp(tests::submit(&h, rq)?);
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.header("location").as_deref(), Some("/elsewhere"));

    let resp = drain_resp(tests::submit(&h, req("/", "worker/location-worker.php"))?);
    assert_eq!(resp.status(), 302);
    drop(h);
    drop(r);
    Ok(())
}

/// Classic mode hits the same populate-before-activate defect and must also answer 303.
#[test]
fn post_location_redirects_303_in_classic_mode() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let mut rq = req("/", "worker/location-classic.php");
    rq.method = "POST".into();
    let resp = drain_resp(tests::submit(&h, rq)?);
    assert_eq!(resp.status(), 303);
    drop(h);
    drop(r);
    Ok(())
}

/// A client that vanishes while queued is discarded before handout: the handler must not run and recycle the worker.
#[test]
fn queued_client_gone_is_discarded_before_handout() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();
    let r = Rapira::start(Mode::Worker(fixture("worker/held-worker.php")))?;
    let h = r.handle();

    let rx_a = tests::submit(&h, req("/", "worker/held-worker.php"))?;
    wait_app_record("held");
    drop(tests::submit(&h, req("/", "worker/held-worker.php"))?);
    let resp_a = drain_resp(rx_a);
    assert_eq!(resp_a.body_string(), "done");

    let resp = drain_resp(tests::submit(
        &h,
        req("/?probe=count", "worker/held-worker.php"),
    )?);
    assert_eq!(resp.body_string(), "runs=1");
    drop(h);
    drop(r);
    Ok(())
}

/// handle_request() from inside its own handler is refused: no deadlock, and the outer job completes.
#[test]
fn nested_handle_request_is_refused() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/nested-worker.php")))?;
    let h = r.handle();
    let mut rx = tests::submit(&h, req("/", "worker/nested-worker.php"))?;
    let resp = tests::drain_resp_deadline(
        &mut rx,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .expect("the outer response never arrived");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.body_string()
            .contains("nested: handle_request() may not be called from inside its handler"),
        "got {:?}",
        resp.body_string()
    );
    drop(h);
    drop(r);
    Ok(())
}
