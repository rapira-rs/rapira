use std::time::{Duration, Instant};

use http::header::{HeaderName, HeaderValue};
use rapira_sapi::Mode;
use serde_json::Value;
use tests::wire::submit;
use tests::{drain, drain_resp, drain_resp_deadline, fixture, req, server_log};

use crate::harness::Spawn;

/// Superglobals are rebuilt per job over the resident loop: query state must not leak.
#[test]
fn worker_serves_with_per_job_superglobals() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("worker/hello-worker.php")).spawn();

    let resp = drain_resp(submit(srv.addr, req("/?q=zap"))?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "hello:GET:zap");
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("text/plain;charset=UTF-8"),
        "header() must reach the head"
    );

    let resp = drain_resp(submit(srv.addr, req("/"))?);
    assert_eq!(
        resp.body_string(),
        "hello:GET:-",
        "query state must not leak"
    );
    Ok(())
}

/// Closing the intake makes handle_request() return false: the post-loop code runs exactly once.
#[test]
fn drain_returns_false_and_the_script_completes() -> anyhow::Result<()> {
    let mut srv = Spawn::http(Mode::Worker, fixture("worker/drain-worker.php"))
        .json_log()
        .spawn();
    for want in ["n=1", "n=2"] {
        let resp = drain_resp(submit(srv.addr, req("/"))?);
        assert_eq!(resp.body_string(), want, "resident state must accumulate");
    }
    srv.stop();

    let exited = server_log::records(&srv.log_file())
        .iter()
        .filter(|c| c.target == "app" && c.message == "loop-exited served=2")
        .count();
    assert_eq!(exited, 1, "the post-loop code must run exactly once");
    Ok(())
}

/// Classic mode: the gate throws `NotInWorkerModeError`, and ZPP rejects a non-callable ahead of it.
#[test]
fn handle_request_outside_worker_mode_throws() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("worker/gate-classic.php")).spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);

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
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture("worker/gate-dispatcher-worker.php"),
    )
    .json_log()
    .spawn();
    let resp = drain_resp(submit(srv.addr, req("/"))?);
    assert_eq!(
        resp.body_string(),
        "ok",
        "the unit must survive the refusal"
    );
    srv.stop();

    let records = server_log::records(&srv.log_file());
    let gated = records
        .iter()
        .filter(|c| {
            c.target == "app" && c.message == "gate Rapira\\Exception\\NotInWorkerModeError"
        })
        .count();
    assert_eq!(gated, 1);
    let finish_gated = records
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
    let srv = Spawn::http(Mode::Worker, fixture("worker/exit-worker.php")).spawn();

    let resp = drain_resp(submit(srv.addr, req("/?die=1"))?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "n=1", "exit must still ship the body");
    let resp = drain_resp(submit(srv.addr, req("/"))?);
    assert_eq!(resp.body_string(), "n=2", "the loop and its state survive");
    Ok(())
}

/// A self-stopping loop classifies Recycle: the next job re-bootstraps and is still served.
#[test]
fn self_stopping_loop_recycles_and_serves_again() -> anyhow::Result<()> {
    let mut srv = Spawn::http(Mode::Worker, fixture("worker/one-turn-worker.php"))
        .json_log()
        .spawn();
    for _ in 0..2 {
        let resp = drain_resp(submit(srv.addr, req("/"))?);
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.body_string(), "once");
    }
    srv.stop();

    let turns = server_log::records(&srv.log_file())
        .iter()
        .filter(|c| c.target == "app" && c.message == "one-turn-done")
        .count();
    assert_eq!(turns, 3, "each bootstrap must run the script to completion");
    Ok(())
}

/// A bootstrap that never calls handle_request() sheds 503 instead of hanging or answering 200.
#[test]
fn never_looping_script_sheds_503() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("worker/never-loop-worker.php")).spawn();
    let mut rx = submit(srv.addr, req("/"))?;
    let resp = drain_resp_deadline(&mut rx, Instant::now() + Duration::from_secs(10))
        .expect("the shed 503 never arrived");
    assert_eq!(resp.status(), 503, "a never-serving bootstrap must shed");
    Ok(())
}

/// Bootstrap $_ENV survives late compilation: php_auto_globals_create_env dtors the array before checking variables_order.
#[test]
fn bootstrap_env_survives_late_compilation() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("worker/env-worker.php")).spawn();
    for job in 0..2 {
        let resp = drain_resp(submit(srv.addr, req("/"))?);
        assert_eq!(resp.body_string(), "set-at-boot", "job {job}");
    }
    Ok(())
}

/// `Location:` on a POST answers 303: sapi_activate resets proto_num, so a missing re-apply degrades it to 302.
#[test]
fn post_location_redirects_303_in_worker_mode() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("worker/location-worker.php")).spawn();
    let mut rq = req("/");
    rq.method = "POST".into();
    let resp = drain_resp(submit(srv.addr, rq)?);
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.header("location").as_deref(), Some("/elsewhere"));

    let resp = drain_resp(submit(srv.addr, req("/"))?);
    assert_eq!(resp.status(), 302);
    Ok(())
}

/// Classic mode hits the same populate-before-activate defect and must also answer 303.
#[test]
fn post_location_redirects_303_in_classic_mode() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("worker/location-classic.php")).spawn();
    let mut rq = req("/");
    rq.method = "POST".into();
    let resp = drain_resp(submit(srv.addr, rq)?);
    assert_eq!(resp.status(), 303);
    Ok(())
}

/// A client that vanishes while queued is discarded before handout: the handler must not run and recycle the worker.
#[test]
fn queued_client_gone_is_discarded_before_handout() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("worker/held-worker.php"))
        .json_log()
        .spawn();

    let rx_a = submit(srv.addr, req("/"))?;
    server_log::wait_app_record(&srv.log_file(), "held");
    let rx_b = submit(srv.addr, req("/"))?;
    // No signal shows that B is queued. The wait lets the plugin read B and queue it before the close, well inside the 400 ms that A holds the worker.
    std::thread::sleep(Duration::from_millis(100));
    drop(rx_b);
    let resp_a = drain_resp(rx_a);
    assert_eq!(resp_a.body_string(), "done");

    let resp = drain_resp(submit(srv.addr, req("/?probe=count"))?);
    assert_eq!(resp.body_string(), "runs=1");
    Ok(())
}

/// handle_request() from inside its own handler is refused: no deadlock, and the outer job completes.
#[test]
fn nested_handle_request_is_refused() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("worker/nested-worker.php")).spawn();
    let mut rx = submit(srv.addr, req("/"))?;
    let resp = drain_resp_deadline(&mut rx, Instant::now() + Duration::from_secs(10))
        .expect("the outer response never arrived");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.body_string()
            .contains("nested: handle_request() may not be called from inside its handler"),
        "got {:?}",
        resp.body_string()
    );
    Ok(())
}

/// $_SERVER keys in registration order: the CGI names of register_server_variables, then HTTP_*, then the keys php_register_server_variables adds.
/// The HTTP_* keys follow the field lines on the wire: the client writes Host first, then the case headers, then Content-Type from `content_type`.
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
            name: "the Host line alone gives the CGI keys and HTTP_HOST",
            content_type: None,
            headers: &[],
            keys: &["CONTENT_LENGTH", "HTTP_HOST"],
            values: &[("CONTENT_LENGTH", "0"), ("AUTH_TYPE", "")],
        },
        Case {
            name: "repeated field lines join into one value",
            content_type: None,
            headers: &[("x-rep", "1"), ("x-rep", "2")],
            keys: &["CONTENT_LENGTH", "HTTP_HOST", "HTTP_X_REP"],
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
                "HTTP_HOST",
                "HTTP_AUTHORIZATION",
                "HTTP_X_A",
                "HTTP_CONTENT_TYPE",
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

    let srv = Spawn::http(Mode::Worker, fixture("worker/server-pairs-worker.php")).spawn();
    for case in &cases {
        let mut rq = req("/");
        rq.content_type = case.content_type.map(Into::into);
        for &(field, value) in case.headers {
            rq.headers.append(
                HeaderName::from_static(field),
                HeaderValue::from_static(value),
            );
        }
        let resp = drain_resp(submit(srv.addr, rq)?);
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
    Ok(())
}
