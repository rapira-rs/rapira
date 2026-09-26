use http::header::{HeaderName, HeaderValue};
use rapira_sapi::{Mode, Rapira};
use serde_json::Value;
use tests::{
    captured, drain, drain_resp, fixture, init_log_capture, php_lock, req, wait_app_record,
};

/// Superglobals are rebuilt per job over the resident loop: query state must not leak.
#[test]
fn worker_serves_with_per_job_superglobals() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/hello-worker.php"),
        None,
    )?;
    let h = r.sink();

    let resp = drain_resp(tests::submit(&h, req("/?q=zap"))?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "hello:GET:zap");
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("text/plain;charset=UTF-8"),
        "header() must reach the head"
    );

    let resp = drain_resp(tests::submit(&h, req("/"))?);
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

    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/drain-worker.php"),
        None,
    )?;
    let h = r.sink();
    for want in ["n=1", "n=2"] {
        let resp = drain_resp(tests::submit(&h, req("/"))?);
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
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Classic,
        fixture("worker/gate-classic.php"),
        None,
    )?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
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

    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Dispatcher,
        fixture("worker/gate-dispatcher-worker.php"),
        Some(rapira_http::DISPATCHER_CLASSES),
    )?;
    let h = r.sink();
    let resp = drain_resp(tests::submit(&h, req("/"))?);
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
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/exit-worker.php"),
        None,
    )?;
    let h = r.sink();

    let resp = drain_resp(tests::submit(&h, req("/?die=1"))?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "n=1", "exit must still ship the body");
    let resp = drain_resp(tests::submit(&h, req("/"))?);
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

    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/one-turn-worker.php"),
        None,
    )?;
    let h = r.sink();
    for _ in 0..2 {
        let resp = drain_resp(tests::submit(&h, req("/"))?);
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
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/never-loop-worker.php"),
        None,
    )?;
    let h = r.sink();
    let mut rx = tests::submit(&h, req("/"))?;
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
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/env-worker.php"),
        None,
    )?;
    let h = r.sink();
    for job in 0..2 {
        let resp = drain_resp(tests::submit(&h, req("/"))?);
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
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/location-worker.php"),
        None,
    )?;
    let h = r.sink();
    let mut rq = req("/");
    rq.method = "POST".into();
    let resp = drain_resp(tests::submit(&h, rq)?);
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.header("location").as_deref(), Some("/elsewhere"));

    let resp = drain_resp(tests::submit(&h, req("/"))?);
    assert_eq!(resp.status(), 302);
    drop(h);
    drop(r);
    Ok(())
}

/// Classic mode hits the same populate-before-activate defect and must also answer 303.
#[test]
fn post_location_redirects_303_in_classic_mode() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Classic,
        fixture("worker/location-classic.php"),
        None,
    )?;
    let h = r.sink();
    let mut rq = req("/");
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
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/held-worker.php"),
        None,
    )?;
    let h = r.sink();

    let rx_a = tests::submit(&h, req("/"))?;
    wait_app_record("held");
    drop(tests::submit(&h, req("/"))?);
    let resp_a = drain_resp(rx_a);
    assert_eq!(resp_a.body_string(), "done");

    let resp = drain_resp(tests::submit(&h, req("/?probe=count"))?);
    assert_eq!(resp.body_string(), "runs=1");
    drop(h);
    drop(r);
    Ok(())
}

/// handle_request() from inside its own handler is refused: no deadlock, and the outer job completes.
#[test]
fn nested_handle_request_is_refused() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/nested-worker.php"),
        None,
    )?;
    let h = r.sink();
    let mut rx = tests::submit(&h, req("/"))?;
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

/// $_SERVER keys in registration order: the CGI names of register_server_variables, then HTTP_*, then the keys php_register_server_variables adds.
/// Header names map as php_register_variable_ex mangles them ('.' to '_'), and a later name that maps to the same key overwrites the value in place.
#[test]
fn server_keys_keep_order_and_mangling() -> anyhow::Result<()> {
    const CGI: &[&str] = &[
        "PHP_SELF",
        "DOCUMENT_URI",
        "DOCUMENT_ROOT",
        "REQUEST_SCHEME",
        "REMOTE_HOST",
        "REMOTE_PORT",
        "REMOTE_IDENT",
        "REQUEST_METHOD",
        "REQUEST_URI",
        "QUERY_STRING",
        "SCRIPT_FILENAME",
        "SCRIPT_NAME",
        "SERVER_PROTOCOL",
        "SERVER_SOFTWARE",
        "SERVER_NAME",
        "SERVER_PORT",
        "REMOTE_ADDR",
        "GATEWAY_INTERFACE",
        "HTTPS",
        "AUTH_TYPE",
    ];
    const TIME: &[&str] = &["REQUEST_TIME_FLOAT", "REQUEST_TIME"];

    struct Case {
        name: &'static str,
        content_type: Option<&'static str>,
        headers: &'static [(&'static str, &'static str)],
        // The keys between AUTH_TYPE and REQUEST_TIME_FLOAT.
        keys: &'static [&'static str],
        values: &'static [(&'static str, &'static str)],
    }
    let cases = [
        Case {
            name: "no header gives only the CGI keys",
            content_type: None,
            headers: &[],
            keys: &["CONTENT_LENGTH"],
            values: &[("CONTENT_LENGTH", "0"), ("AUTH_TYPE", "")],
        },
        Case {
            name: "dot in a field name maps to underscore",
            content_type: None,
            headers: &[("x.dot", "a")],
            keys: &["CONTENT_LENGTH", "HTTP_X_DOT"],
            values: &[("HTTP_X_DOT", "a")],
        },
        Case {
            name: "dot and dash names share one key and the later one wins",
            content_type: None,
            headers: &[("x.dot", "a"), ("x-dot", "b")],
            keys: &["CONTENT_LENGTH", "HTTP_X_DOT"],
            values: &[("HTTP_X_DOT", "b")],
        },
        Case {
            name: "repeated field lines join into one value",
            content_type: None,
            headers: &[("x-rep", "1"), ("x-rep", "2")],
            keys: &["CONTENT_LENGTH", "HTTP_X_REP"],
            values: &[("HTTP_X_REP", "1, 2")],
        },
        Case {
            name: "every optional key in its place",
            content_type: Some("text/plain"),
            headers: &[
                ("authorization", "Basic dmFsZXJ5OnBhc3N3b3Jk"),
                ("x-a", "1"),
            ],
            keys: &[
                "REMOTE_USER",
                "CONTENT_TYPE",
                "CONTENT_LENGTH",
                "HTTP_AUTHORIZATION",
                "HTTP_X_A",
                "PHP_AUTH_USER",
                "PHP_AUTH_PW",
            ],
            values: &[
                ("AUTH_TYPE", "Basic"),
                ("REMOTE_USER", "valery"),
                ("CONTENT_TYPE", "text/plain"),
                ("PHP_AUTH_USER", "valery"),
                ("PHP_AUTH_PW", "password"),
            ],
        },
    ];

    let _guard = php_lock();
    let r = Rapira::start(
        &tests::PHP_PARTS,
        Mode::Worker,
        fixture("worker/server-pairs-worker.php"),
        None,
    )?;
    let h = r.sink();
    for case in &cases {
        let mut rq = req("/");
        rq.content_type = case.content_type.map(Into::into);
        for &(field, value) in case.headers {
            rq.headers.append(
                HeaderName::from_static(field),
                HeaderValue::from_static(value),
            );
        }
        let resp = drain_resp(tests::submit(&h, rq)?);
        let pairs: Vec<(String, Value)> = serde_json::from_str(&resp.body_string())?;
        // With register_argc_argv = 1 (the PHP 8.4 default; 8.5 defaults to 0) the engine appends argv and argc after REQUEST_TIME.
        // https://www.php.net/manual/en/ini.core.php#ini.register-argc-argv
        let pairs: Vec<(String, Value)> = pairs
            .into_iter()
            .filter(|(k, _)| k != "argv" && k != "argc")
            .collect();

        let got: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        let want: Vec<&str> = [CGI, case.keys, TIME].concat();
        assert_eq!(got, want, "{}", case.name);

        for (key, value) in &pairs {
            match key.as_str() {
                "REQUEST_TIME_FLOAT" => assert!(value.is_f64(), "{}: {key}", case.name),
                "REQUEST_TIME" => assert!(value.is_i64(), "{}: {key}", case.name),
                _ => assert!(value.is_string(), "{}: {key}", case.name),
            }
        }
        for &(key, want) in case.values {
            let got = pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v);
            assert_eq!(got, Some(&Value::from(want)), "{}: {key}", case.name);
        }
    }
    drop(h);
    drop(r);
    Ok(())
}
