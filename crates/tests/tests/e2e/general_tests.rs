use rapira_sapi::{Mode, Request};
use tests::wire::submit;
use tests::{drain, drain_resp, fixture, req, server_log};

use crate::harness::{Server, Spawn, slot_line};

fn post(body: Vec<u8>) -> Request {
    let mut r: Request = req("/");
    r.method = "POST".into();
    r.content_type = Some("text/plain".into());
    r.body = rapira_sapi::types::Body::Raw(std::io::Cursor::new(body));
    r
}

// PHP core ignores ub_write's return value, so the SAPI must raise php_handle_aborted_connection() itself, and the aborted status must not leak into the next request.
#[test]
fn client_disconnect_aborts_request() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("general_tests/abort-worker.php"))
        .json_log()
        .spawn();

    let rx = submit(srv.addr, req("/"))?;
    server_log::wait_app_record(&srv.log_file(), "held");
    drop(rx);

    let (s2, b2) = drain(submit(srv.addr, req("/?probe=1"))?);

    assert_eq!(s2, 200, "worker must survive the aborted request");
    assert!(
        b2.contains("done=0"),
        "work after the disconnect must not run (got: {b2:?})"
    );
    assert!(
        b2.contains("aborted=0"),
        "connection status must reset for the next request (got: {b2:?})"
    );
    Ok(())
}

// sapi_deactivate_module() only NULLs SG(request_info).request_body, so without a sweep every POST in a resident worker grows EG(regular_list) by one temp stream.
#[test]
fn post_temp_streams_do_not_accumulate() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("general_tests/resources-worker.php")).spawn();

    let send = |srv: &Server| -> anyhow::Result<i64> {
        let (_, b) = drain(submit(srv.addr, post(b"x=1".to_vec()))?);
        b.split_once("streams=")
            .and_then(|(_, n)| n.trim().parse().ok())
            .ok_or_else(|| anyhow::anyhow!("fixture must print streams=N (got: {b:?})"))
    };

    let first = send(&srv)?;
    send(&srv)?;
    send(&srv)?;
    let fourth = send(&srv)?;

    assert_eq!(
        first, fourth,
        "stream resources must not accumulate across POST requests"
    );
    Ok(())
}

// The worker path must run userland set_exception_handler like zend_execute_scripts does, and a handled exception must not raise a 500 or a scoreboard error.
#[test]
fn uncaught_throwable_reaches_exception_handler() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("general_tests/exception-handler-worker.php"),
    )
    .spawn();
    let (s1, b1) = drain(submit(srv.addr, req("/"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/"))?);
    // The slot counts a job before its response goes out, so both jobs are counted here.
    let slot = slot_line(&srv, "handled 2 errors");

    assert_eq!(s1, 200);
    assert!(
        b1.contains("handled:boom") && !b1.contains("Uncaught"),
        "set_exception_handler must receive the throwable (got: {b1:?})"
    );
    assert_eq!(s2, 200);
    assert!(
        b2.contains("handled:boom"),
        "the handler persists on the worker (got: {b2:?})"
    );
    assert!(
        slot.contains("errors 0 "),
        "a handled exception is not an engine error (got: {slot:?})"
    );
    Ok(())
}

// With display_errors=0 an uncaught throw emits nothing before the error path, and the worker answers it with one complete 500.
#[test]
fn quiet_uncaught_throw_answers_a_complete_500() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("general_tests/throw-quiet-worker.php"),
    )
    .spawn();

    let resp = drain_resp(submit(srv.addr, req("/"))?);

    assert!(resp.ended && !resp.truncated, "worker must seal a response");
    let head = resp.head.expect("error response must record a head");
    assert_eq!(
        head.status, 500,
        "uncaught throw with display_errors=0 is a 500"
    );
    Ok(())
}

// RSHUTDOWN wraps php_session_flush in its own zend_try; without that guard a bailing save handler skips the rest of the reset and the next request reuses the session id.
#[test]
fn session_reset_survives_bailing_save_handler() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/session-bailout-worker.php")).spawn();
    let (_, b1) = drain(submit(srv.addr, req("/"))?);
    let (_, b2) = drain(submit(srv.addr, req("/"))?);

    let sid = |b: &str| {
        b.split_whitespace()
            .find_map(|t| t.strip_prefix("sid=").map(str::to_owned))
    };
    assert!(
        sid(&b1).is_some(),
        "req1 must start a session (got: {b1:?})"
    );
    assert_ne!(
        sid(&b1),
        sid(&b2),
        "a bailing save handler must not leave the previous session active (b1={b1:?}, b2={b2:?})"
    );
    Ok(())
}

#[test]
fn fatal_in_exception_handler_keeps_worker_alive() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("general_tests/fatal-exception-handler-worker.php"),
    )
    .spawn();
    let (s1, _) = drain(submit(srv.addr, req("/"))?);
    assert!(s1 == 200, "req1 must return a head, not hang (got {s1})");
    let (s2, b2) = drain(submit(srv.addr, req("/"))?);
    // The plugin answers a stopped intake with an empty 500, so a 500 counts only with the fatal text that PHP prints.
    assert!(
        s2 == 200 || (s2 == 500 && !b2.is_empty()),
        "worker must survive and serve req2 (got {s2}, body {b2:?})"
    );
    Ok(())
}

#[test]
fn fatal_backtrace_freed_between_requests() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("general_tests/fatal-backtrace-worker.php"),
    )
    .spawn();
    let mem = |b: String| -> i64 {
        b.trim()
            .strip_prefix("mem=")
            .and_then(|s| s.parse().ok())
            .expect("mem= output")
    };
    let b0 = mem(drain(submit(srv.addr, req("/?step=probe"))?).1);
    let (_, boom) = drain(submit(srv.addr, req("/?step=boom"))?);
    assert!(
        boom.contains("boomed"),
        "error consumed + execution continued (got {boom:?})"
    );
    let leaked = mem(drain(submit(srv.addr, req("/?step=probe"))?).1) - b0;
    assert!(
        leaked < 5 * 1024 * 1024,
        "fatal backtrace must be freed between jobs; {leaked} bytes still pinned (~20MB pre-fix)"
    );
    Ok(())
}

#[test]
fn shutdown_function_fatal_recycles_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/shutdown-fatal-worker.php")).spawn();
    let (_, b1) = drain(submit(srv.addr, req("/?boom=1"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/"))?);
    assert!(b1.contains("ok counter=1"), "req1 baseline (got: {b1:?})");
    assert_eq!(s2, 200, "worker must survive (got {s2})");
    assert!(
        b2.contains("ok counter=1"),
        "fatal in shutdown fn must recycle, resetting statics (got: {b2:?})"
    );
    Ok(())
}

#[test]
fn client_disconnect_respects_ignore_user_abort() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("general_tests/abort-ignore-worker.php"),
    )
    .json_log()
    .spawn();
    let rx = submit(srv.addr, req("/"))?;
    server_log::wait_app_record(&srv.log_file(), "held");
    drop(rx);
    let (s2, b2) = drain(submit(srv.addr, req("/?probe=1"))?);
    assert_eq!(s2, 200, "worker must survive the ignored abort");
    assert!(
        b2.contains("reached=1"),
        "work after disconnect must still run (got: {b2:?})"
    );
    assert!(
        b2.contains("aborted=0"),
        "connection status must reset (got: {b2:?})"
    );
    Ok(())
}
