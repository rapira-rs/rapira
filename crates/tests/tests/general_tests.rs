use std::io::Read;

use php_sys::{Mode, Rapira, Request};
use tests::{
    captured, drain, drain_resp, fixture, init_log_capture, php_lock, req, wait_app_record,
};

/// Body source returning at most one byte per read() call, legal `Read` behavior for streaming bodies.
struct Trickle(std::io::Cursor<Vec<u8>>);

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let end = buf.len().min(1);
        self.0.read(&mut buf[..end])
    }
}

fn post(fixture_name: &str, body: Box<dyn Read + Send>, len: i64) -> Request {
    let mut r: Request = req("/", fixture_name);
    r.method = "POST".into();
    r.content_type = Some("text/plain".into());
    r.content_length = len;
    r.body = php_sys::types::Body::Raw(body);
    r
}

// php-src treats any short read_post() return as end-of-body (SG(post_read)=1, main/SAPI.c), so a callback that does not fill the buffer truncates the POST body.
#[test]
fn post_body_survives_partial_reads() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("general_tests/input-worker.php")))?;
    let h = r.handle();

    let payload = b"hello rapira post".to_vec();
    let len = payload.len() as i64;
    let request = post(
        "general_tests/input-worker.php",
        Box::new(Trickle(std::io::Cursor::new(payload))),
        len,
    );
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    assert!(
        body.contains("len=17") && body.contains("body=hello rapira post"),
        "php://input must see the whole trickled body (got: {body:?})"
    );
    Ok(())
}

// PHP core ignores ub_write's return value, so the SAPI must raise php_handle_aborted_connection() itself, and the aborted status must not leak into the next request.
#[test]
fn client_disconnect_aborts_request() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();
    let r = Rapira::start(Mode::Worker(fixture("general_tests/abort-worker.php")))?;
    let h = r.handle();

    let rx = h.handle_blocking(req("/", "general_tests/abort-worker.php"))?;
    wait_app_record("held");
    drop(rx);

    let (s2, b2) = drain(h.handle_blocking(req("/?probe=1", "general_tests/abort-worker.php"))?);
    drop(h);
    r.shutdown();

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
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("general_tests/resources-worker.php")))?;
    let h = r.handle();

    let send = |h: &php_sys::RapiraHandle| -> anyhow::Result<i64> {
        let body = b"x=1".to_vec();
        let len = body.len() as i64;
        let (_, b) = drain(h.handle_blocking(post(
            "general_tests/resources-worker.php",
            Box::new(std::io::Cursor::new(body)),
            len,
        ))?);
        b.split_once("streams=")
            .and_then(|(_, n)| n.trim().parse().ok())
            .ok_or_else(|| anyhow::anyhow!("fixture must print streams=N (got: {b:?})"))
    };

    let first = send(&h)?;
    send(&h)?;
    send(&h)?;
    let fourth = send(&h)?;
    drop(h);
    r.shutdown();

    assert_eq!(
        first, fourth,
        "stream resources must not accumulate across POST requests"
    );
    Ok(())
}

#[test]
fn https_server_vars() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/server-variables.php")))?;
    let h = r.handle();
    let mut request = req("/", "shared/server-variables.php");
    request.https = true;
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    assert!(
        body.contains("[HTTPS] => on"),
        "TLS request must set $_SERVER['HTTPS']=on (got: {body:?})"
    );
    assert!(
        body.contains("[GATEWAY_INTERFACE] => CGI/1.1"),
        "got: {body:?}"
    );
    Ok(())
}

// The worker path must run userland set_exception_handler like zend_execute_scripts does, and a handled exception must not raise a 500 or a scoreboard error.
#[test]
fn uncaught_throwable_reaches_exception_handler() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "general_tests/exception-handler-worker.php",
    )))?;
    let h = r.handle();
    let (s1, b1) =
        drain(h.handle_blocking(req("/", "general_tests/exception-handler-worker.php"))?);
    let (s2, b2) =
        drain(h.handle_blocking(req("/", "general_tests/exception-handler-worker.php"))?);
    drop(h);
    let snap = r.scoreboard();
    r.shutdown();

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
    assert_eq!(snap.errors, 0, "a handled exception is not an engine error");
    Ok(())
}

// With display_errors=0 an uncaught throw emits nothing before the error path, so the rust-side 500 head and the teardown header flush must not both head the response.
#[test]
fn error_response_sends_exactly_one_head() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "general_tests/throw-quiet-worker.php",
    )))?;
    let h = r.handle();

    let resp = drain_resp(h.handle_blocking(req("/", "general_tests/throw-quiet-worker.php"))?);
    drop(h);
    r.shutdown();

    assert!(resp.ended, "worker must seal a response");
    assert_eq!(
        resp.heads, 1,
        "the 500 and the teardown flush must not both head"
    );
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
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/session-bailout-worker.php")))?;
    let h = r.handle();
    let (_, b1) = drain(h.handle_blocking(req("/", "shared/session-bailout-worker.php"))?);
    let (_, b2) = drain(h.handle_blocking(req("/", "shared/session-bailout-worker.php"))?);
    drop(h);
    r.shutdown();

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
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "general_tests/fatal-exception-handler-worker.php",
    )))?;
    let h = r.handle();
    let (s1, _) =
        drain(h.handle_blocking(req("/", "general_tests/fatal-exception-handler-worker.php"))?);
    assert!(s1 == 200, "req1 must return a head, not hang (got {s1})");
    let (s2, _) =
        drain(h.handle_blocking(req("/", "general_tests/fatal-exception-handler-worker.php"))?);
    assert!(
        s2 == 200 || s2 == 500,
        "worker must survive and serve req2 (got {s2})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

// Smoke coverage, not a strict guard for module.c's PG(in_user_include)=0: the req1 bailout forces a recycle whose php_request_startup already re-zeroes the flag.
#[test]
fn in_user_include_flag_reset_between_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("general_tests/stuck-flag-worker.php")))?;
    let h = r.handle();
    let _ = drain(h.handle_blocking(req("/?step=boom", "general_tests/stuck-flag-worker.php"))?);
    let (_, b2) = drain(h.handle_blocking(req("/", "general_tests/stuck-flag-worker.php"))?);
    assert!(
        b2.contains("PROBE_OK"),
        "worker recovers; data:// is not rejected as an include (got {b2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn fatal_backtrace_freed_between_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "general_tests/fatal-backtrace-worker.php",
    )))?;
    let h = r.handle();
    let mem = |b: String| -> i64 {
        b.trim()
            .strip_prefix("mem=")
            .and_then(|s| s.parse().ok())
            .expect("mem= output")
    };
    let b0 = mem(drain(h.handle_blocking(req(
        "/?step=probe",
        "general_tests/fatal-backtrace-worker.php",
    ))?)
    .1);
    let (_, boom) = drain(h.handle_blocking(req(
        "/?step=boom",
        "general_tests/fatal-backtrace-worker.php",
    ))?);
    assert!(
        boom.contains("boomed"),
        "error consumed + execution continued (got {boom:?})"
    );
    let leaked = mem(drain(h.handle_blocking(req(
        "/?step=probe",
        "general_tests/fatal-backtrace-worker.php",
    ))?)
    .1) - b0;
    assert!(
        leaked < 5 * 1024 * 1024,
        "fatal backtrace must be freed between jobs; {leaked} bytes still pinned (~20MB pre-fix)"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn shutdown_function_fatal_recycles_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/shutdown-fatal-worker.php")))?;
    let h = r.handle();
    let (_, b1) = drain(h.handle_blocking(req("/?boom=1", "shared/shutdown-fatal-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "shared/shutdown-fatal-worker.php"))?);
    drop(h);
    r.shutdown();
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
    let _guard = php_lock();
    init_log_capture();
    captured().clear();
    let r = Rapira::start(Mode::Worker(fixture(
        "general_tests/abort-ignore-worker.php",
    )))?;
    let h = r.handle();
    let rx = h.handle_blocking(req("/", "general_tests/abort-ignore-worker.php"))?;
    wait_app_record("held");
    drop(rx);
    let (s2, b2) =
        drain(h.handle_blocking(req("/?probe=1", "general_tests/abort-ignore-worker.php"))?);
    drop(h);
    r.shutdown();
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
