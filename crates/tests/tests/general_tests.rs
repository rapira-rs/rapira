use std::io::Read;

use php_sys::{Mode, Rapira, Request};
use tests::{drain, fixture, php_lock, req};

/// Body source returning at most one byte per read() call — legal `Read`
/// behavior that streaming bodies (pipes, chunked decoders) exhibit.
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
    r.body = body;
    r
}

// php-src treats any short read_post() return as end-of-body
// (SG(post_read)=1, main/SAPI.c) - the callback must fill the buffer until
// real EOF or partial reads truncate the POST body.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn post_body_survives_partial_reads() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/input-worker.php",
    )))?;
    let h = r.handle()?;

    let payload = b"hello rapira post".to_vec(); // 17 bytes
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

// dropping the response receiver = client disconnect. PHP core ignores
// ub_write's return value, so the SAPI must raise
// php_handle_aborted_connection() itself: the handler's remaining work is cut
// short (default ignore_user_abort=0), and the ABORTED status must not leak
// into the next request.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn client_disconnect_aborts_request() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/abort-worker.php",
    )))?;
    let h = r.handle()?;

    // Drop the receiver before the fixture's first write (it sleeps to hand us
    // the window): the write then observes the closed channel and aborts.
    drop(h.handle_blocking(req("/", "general_tests/abort-worker.php"))?); // client disconnects

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

// sapi_deactivate_module() only NULLs SG(request_info).request_body; in a
// resident worker request nothing reclaims the temp stream resource, so every
// POST grows EG(regular_list) until a sweep is in place.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn post_temp_streams_do_not_accumulate() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/resources-worker.php",
    )))?;
    let h = r.handle()?;

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
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn https_server_vars() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture("shared/server-variables.php")))?;
    let h = r.handle()?;
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

// classic mode runs userland set_exception_handler for uncaught
// throwables (zend_execute_scripts); the worker path must do the same. A
// handled exception is not an error: no 500, no scoreboard error.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn uncaught_throwable_reaches_exception_handler() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/exception-handler-worker.php",
    )))?;
    let h = r.handle()?;
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

// exactly one head per response. With display_errors=0 an uncaught throw
// produces no output before the error path, so the rust-side 500 head and the
// teardown header flush must not fight over it (first write wins).
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn error_response_sends_exactly_one_head() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/throw-quiet-worker.php",
    )))?;
    let h = r.handle()?;

    let mut rx = h.handle_blocking(req("/", "general_tests/throw-quiet-worker.php"))?;
    let frame = rx.blocking_recv().expect("worker must seal a response");
    assert!(
        rx.blocking_recv().is_none(),
        "exactly one frame per response"
    );
    drop(h);
    r.shutdown();

    let head = frame.head.expect("error response must record a head");
    assert_eq!(
        head.status, 500,
        "uncaught throw with display_errors=0 is a 500"
    );
    Ok(())
}

// RSHUTDOWN wraps php_session_flush alone in zend_try so a
// bailing save handler cannot skip the rest of the reset. Without the inner
// guard the bailed flush leaves the session active and the next request
// reuses the previous session id.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn session_reset_survives_bailing_save_handler() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "shared/session-bailout-worker.php",
    )))?;
    let h = r.handle()?;
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
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn fatal_in_exception_handler_keeps_worker_alive() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/fatal-exception-handler-worker.php",
    )))?;
    let h = r.handle()?;
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

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn in_user_include_flag_reset_between_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/stuck-flag-worker.php",
    )))?;
    let h = r.handle()?;
    // req1: fatal inside the include-wrapper -> bailout strands in_user_include (returning proves no hang)
    let _ = drain(h.handle_blocking(req("/?step=boom", "general_tests/stuck-flag-worker.php"))?);
    // Smoke coverage, not a strict guard for module.c's PG(in_user_include)=0: the
    // req1 bailout forces a recycle whose php_request_startup already re-zeroes the
    // flag, so req2 would pass even if that reset were reverted.
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
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn fatal_backtrace_freed_between_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/fatal-backtrace-worker.php",
    )))?;
    let h = r.handle()?;
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
    // consumed fatal: execution continues, frame unwinds, backtrace is the sole ref to the 20MB
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
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn shutdown_function_fatal_recycles_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "shared/shutdown-fatal-worker.php",
    )))?;
    let h = r.handle()?;
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
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn client_disconnect_respects_ignore_user_abort() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "general_tests/abort-ignore-worker.php",
    )))?;
    let h = r.handle()?;
    // Drop the receiver before the fixture's write (it sleeps to hand us the
    // window): the write observes the closed channel and raises the abort.
    drop(h.handle_blocking(req("/", "general_tests/abort-ignore-worker.php"))?); // client disconnects
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
