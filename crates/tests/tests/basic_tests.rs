use php_sys::{Mode, Rapira};
use tests::{captured, drain, fixture, init_log_capture, php_lock, req};

#[test]
fn fibers_stress_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "basic_tests/fibers.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(
        status, 200,
        "fiber script must compile + run without a stack-guard fatal (got {status}, body {body:?})"
    );
    assert!(
        body.contains("fibers ok sum=226644"),
        "fibers must complete with the correct total (got: {body:?})"
    );
    Ok(())
}

#[test]
fn worker_request_isolation() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/leak-worker.php")))?;
    let h = r.handle();
    let (_, body1) = drain(h.handle_blocking(req("/?x=1", "shared/leak-worker.php"))?);
    let (_, body2) = drain(h.handle_blocking(req("/?x=2", "shared/leak-worker.php"))?);
    assert!(
        body1.contains("counter=1") && body1.contains("session=clean"),
        "req1 baseline (got: {body1:?})"
    );
    assert!(
        body2.contains("session=clean"),
        "$_SESSION must reset between requests (got: {body2:?})"
    );
    assert!(
        body2.contains("counter=2"),
        "static class props persist across requests by design (got: {body2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn fibers_stress_worker() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("shared/fibers-worker.php")))?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "shared/fibers-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(
        status, 200,
        "fiber script must compile + run without a stack-guard fatal (got {status}, body {body:?})"
    );
    assert!(
        body.contains("fibers ok sum=226644"),
        "fibers must complete with the correct total (got: {body:?})"
    );
    Ok(())
}

#[test]
fn worker_survives_teardown_bailout() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("shared/teardown-bailout-worker.php")))?;
    let h = r.handle();
    let (s1, b1) = drain(h.handle_blocking(req("/?boom=0", "shared/teardown-bailout-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/?boom=1", "shared/teardown-bailout-worker.php"))?);
    let (s3, b3) = drain(h.handle_blocking(req("/?boom=0", "shared/teardown-bailout-worker.php"))?);

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 baseline (got: {b1:?})");

    assert_eq!(
        s2, 500,
        "teardown-flush bailout commits a 500 head (got {s2}, body {b2:?})"
    );
    assert!(
        b2.is_empty(),
        "buffered body is lost to the bailout (got: {b2:?})"
    );

    assert_eq!(
        s3, 200,
        "worker must recover after a teardown bailout (got {s3})"
    );
    assert!(
        b3.contains("ok counter=1"),
        "recycle re-runs the bootstrap; statics reset (got: {b3:?})"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn worker_basic_auth() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("shared/auth-worker.php")))?;
    let h = r.handle();

    let mut with_auth = req("/", "shared/auth-worker.php");
    with_auth
        .headers
        .push(("Authorization".into(), "Basic dXNlcjpwYXNz".into()));
    let (s_auth, b_auth) = drain(h.handle_blocking(with_auth)?);

    let (s_none, b_none) = drain(h.handle_blocking(req("/", "shared/auth-worker.php"))?);

    drop(h);
    r.shutdown();

    assert_eq!(s_auth, 200);
    assert!(
        b_auth.contains("user=user") && b_auth.contains("pass=pass"),
        "Basic auth must populate PHP_AUTH_USER/PW (got: {b_auth:?})",
    );

    assert_eq!(s_none, 200);
    assert!(
        b_none.contains("user=- pass=-"),
        "no auth header -> no PHP_AUTH vars (got: {b_none:?})",
    );

    Ok(())
}

#[test]
fn server_variables() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r: Rapira = Rapira::start(Mode::Worker(fixture("shared/server-variables.php")))?;
    let h: php_sys::RapiraHandle = r.handle();

    let mut request: php_sys::Request = req(
        "/server-variables.php?foo=a&bar=b",
        "shared/server-variables.php",
    );
    request.method = "POST".into();
    request.content_type = Some("text/plain".into());
    request.content_length = 3;
    request.body = php_sys::types::Body::Raw(std::io::Cursor::new(b"foo".to_vec()));
    request
        .headers
        .push(("Authorization".into(), "Basic dmFsZXJ5OnBhc3N3b3Jk".into()));

    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    for expected in [
        "[REQUEST_METHOD] => POST",
        "[QUERY_STRING] => foo=a&bar=b",
        "[REQUEST_URI] => /server-variables.php?foo=a&bar=b",
        "[CONTENT_TYPE] => text/plain",
        "[CONTENT_LENGTH] => 3",
        "[REMOTE_ADDR] => 127.0.0.1",
        "[SERVER_NAME] => localhost",
        "[SERVER_PORT] => 8080",
        "[SERVER_PROTOCOL] => HTTP/1.1",
        "[SERVER_SOFTWARE] => Rapira",
        "[HTTP_AUTHORIZATION] => Basic dmFsZXJ5OnBhc3N3b3Jk",
        "[PHP_AUTH_USER] => valery",
        "[PHP_AUTH_PW] => password",
    ] {
        assert!(
            body.contains(expected),
            "$_SERVER missing {expected:?} (got: {body:?})"
        );
    }
    for expected in [
        format!(
            "[SCRIPT_FILENAME] => {}\n",
            fixture("shared/server-variables.php").display()
        ),
        format!("[DOCUMENT_ROOT] => {}\n", fixture("shared").display()),
        "[SCRIPT_NAME] => /server-variables.php\n".into(),
        "[PHP_SELF] => /server-variables.php\n".into(),
    ] {
        assert!(
            body.contains(&expected),
            "$_SERVER missing {expected:?} (got: {body:?})"
        );
    }
    Ok(())
}

#[test]
fn worker_finish_request() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("shared/finish-request-worker.php")))?;
    let h = r.handle();

    let (s1, b1) = drain(h.handle_blocking(req("/", "shared/finish-request-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "shared/finish-request-worker.php"))?);

    drop(h);
    r.shutdown();

    assert_eq!(s1, 200);
    assert!(
        b1.contains("count=0") && b1.contains("BEFORE"),
        "pre-finish output must reach the client (got: {b1:?})"
    );
    assert!(
        !b1.contains("AFTER"),
        "post-finish output must NOT reach the client (got: {b1:?})"
    );

    assert_eq!(s2, 200);
    assert!(
        b2.contains("count=1"),
        "work after rapira_finish_request() must still execute (got: {b2:?})"
    );
    assert!(
        !b2.contains("AFTER"),
        "post-finish output stays dropped on the next request too (got: {b2:?})"
    );

    Ok(())
}

#[test]
fn getenv_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    unsafe {
        std::env::set_var("FOO", "BAR");
    }
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "basic_tests/env.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn getenv_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    unsafe {
        std::env::set_var("FOO", "BAR");
    }
    let r = Rapira::start(Mode::Worker(fixture("shared/env-worker.php")))?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "shared/env-worker.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn failboot_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "basic_tests/failboot.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(status, 200);
    assert!(
        body.contains("syntax error, unexpected end of file, expecting variable or"),
        "expected error trace (got: {body:?})"
    );
    Ok(())
}

#[test]
fn scoreboard_counts_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/throw-worker.php")))?;
    let h = r.handle();
    let _ = drain(h.handle_blocking(req("/?boom=0", "shared/throw-worker.php"))?);
    let _ = drain(h.handle_blocking(req("/?boom=0", "shared/throw-worker.php"))?);
    let _ = drain(h.handle_blocking(req("/?boom=1", "shared/throw-worker.php"))?);
    drop(h);
    let snap = r.scoreboard().expect("private scoreboard slot");
    r.shutdown();

    assert_eq!(snap.handled, 3, "3 requests handled");
    assert_eq!(snap.errors, 1, "one engine error (uncaught throw)");
    Ok(())
}

#[test]
fn scoreboard_counts_recycles_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/shutdown-fatal-worker.php")))?;
    let h = r.handle();
    let _ = drain(h.handle_blocking(req("/?boom=1", "shared/shutdown-fatal-worker.php"))?);
    let (s2, _) = drain(h.handle_blocking(req("/", "shared/shutdown-fatal-worker.php"))?);
    drop(h);
    let snap = r.scoreboard().expect("private scoreboard slot");
    r.shutdown();

    assert_eq!(s2, 200, "worker recovers after the recycle");
    assert_eq!(snap.handled, 2, "both jobs handled");
    assert!(
        snap.recycles >= 1,
        "the shutdown-fn fatal must recycle the worker (recycles={})",
        snap.recycles
    );
    Ok(())
}

#[test]
fn scoreboard_counts_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let _ = drain(h.handle_blocking(req("/", "shared/hello.php"))?);
    let _ = drain(h.handle_blocking(req("/", "shared/hello.php"))?);
    let _ = drain(h.handle_blocking(req("/", "basic_tests/failboot.php"))?);
    drop(h);
    let snap = r.scoreboard().expect("private scoreboard slot");
    r.shutdown();

    assert_eq!(snap.handled, 3, "3 requests handled");
    assert_eq!(
        snap.errors, 1,
        "one PHP error (failboot.php fails to compile)"
    );
    Ok(())
}

#[test]
fn worker_session_isolation() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/session-worker.php")))?;
    let h = r.handle();
    let (s1, b1) = drain(h.handle_blocking(req("/", "shared/session-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "shared/session-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 200);
    assert_eq!(s2, 200);
    assert!(b1.contains("n=0"), "req1 fresh session (got: {b1:?})");
    assert!(
        b2.contains("n=0"),
        "session must reset between worker requests (got: {b2:?})"
    );
    let sid = |b: &str| {
        b.split_whitespace()
            .find_map(|t| t.strip_prefix("sid=").map(str::to_owned))
    };
    assert_ne!(
        sid(&b1),
        sid(&b2),
        "each request must get a fresh session id (b1={b1:?}, b2={b2:?})"
    );
    Ok(())
}

#[test]
fn worker_bootstrap_output_is_logged() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("basic_tests/boot-output-worker.php")))?;
    let h = r.handle();
    let (status, _) = drain(h.handle_blocking(req("/", "basic_tests/boot-output-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "worker still serves after no-context output");
    let logged = captured();
    assert!(
        logged
            .iter()
            .any(|c| c.message.contains("WORKER-BOOT-OUTPUT")),
        "worker bootstrap output must be logged (captured: {logged:?})"
    );
    Ok(())
}

fn php_levels(logged: &[tests::Captured], mark: &str) -> Vec<tracing::Level> {
    logged
        .iter()
        .filter(|c| c.target == "php" && c.message.contains(mark))
        .map(|c| c.level)
        .collect()
}

#[test]
fn php_diagnostics_log_at_their_error_type_level() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("basic_tests/error-levels-worker.php")))?;
    let h = r.handle();
    for step in ["deprecated", "warn", "boom"] {
        let uri = format!("/?step={step}");
        let _ = drain(h.handle_blocking(req(&uri, "basic_tests/error-levels-worker.php"))?);
    }
    drop(h);
    r.shutdown();

    let logged = captured();
    assert_eq!(
        php_levels(&logged, "MASKED-DEPRECATION"),
        vec![tracing::Level::TRACE],
        "a diagnostic the script masked must not reach error (captured: {logged:?})"
    );
    assert_eq!(
        php_levels(&logged, "REPORTED-WARNING"),
        vec![tracing::Level::WARN, tracing::Level::WARN],
        "an unmasked E_USER_WARNING logs at warn (captured: {logged:?})"
    );
    assert_eq!(
        php_levels(&logged, "REPORTED-FATAL"),
        vec![tracing::Level::ERROR, tracing::Level::ERROR],
        "an uncaught throw stays an error-level diagnostic (captured: {logged:?})"
    );
    Ok(())
}

/// Fatals are exempt from the error_reporting(0) mask: the recycle they cause still has to be explained.
#[test]
fn masked_fatal_still_logs_at_error() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("basic_tests/error-levels-worker.php")))?;
    let h = r.handle();
    let _ = drain(h.handle_blocking(req(
        "/?step=silent-fatal",
        "basic_tests/error-levels-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    let logged = captured();
    assert_eq!(
        php_levels(&logged, "SILENCED-FATAL"),
        vec![tracing::Level::ERROR],
        "a fatal stays visible however the script masks it (captured: {logged:?})"
    );
    Ok(())
}

/// log_errors routes a deprecation through the SAPI log callback too; both paths must report debug.
#[test]
fn logged_deprecation_stays_at_debug_on_both_paths() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("basic_tests/error-levels-worker.php")))?;
    let h = r.handle();
    let (status, _) =
        drain(h.handle_blocking(req("/?step=logged", "basic_tests/error-levels-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    let logged = captured();
    assert_eq!(
        php_levels(&logged, "LOGGED-DEPRECATION"),
        vec![tracing::Level::DEBUG, tracing::Level::DEBUG],
        "the log callback reports a deprecation at debug (captured: {logged:?})"
    );
    Ok(())
}

#[test]
fn sapi_ini_entries_applied() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "basic_tests/ini.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    assert!(
        body.contains("met=0"),
        "ini_entries must apply: max_execution_time=0 (got: {body:?})"
    );
    Ok(())
}

#[test]
fn status_code_does_not_leak_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("basic_tests/status-worker.php")))?;
    let h = r.handle();
    let (s1, _) = drain(h.handle_blocking(req("/?code=404", "basic_tests/status-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "basic_tests/status-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 404, "explicit http_response_code(404)");
    assert_eq!(
        s2, 200,
        "default status must be 200, not the leaked 404 (body: {b2:?})"
    );
    Ok(())
}

#[test]
fn status_code_does_not_leak_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (s1, _) = drain(h.handle_blocking(req("/", "basic_tests/status-404.php"))?);
    let (s2, _) = drain(h.handle_blocking(req("/", "shared/hello.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 404);
    assert_eq!(
        s2, 200,
        "classic mode reuses SG on the thread; 404 must not leak"
    );
    Ok(())
}

#[test]
fn worker_finish_request_header_only() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "basic_tests/finish-request-headers-worker.php",
    )))?;
    let h = r.handle();
    let (status, body) =
        drain(h.handle_blocking(req("/", "basic_tests/finish-request-headers-worker.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(status, 302);
    assert!(body.is_empty());
    Ok(())
}

#[test]
fn teardown_bailout_does_not_leave_gc_protected() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("basic_tests/gc-protect-worker.php")))?;
    let h = r.handle();
    let (_, b1) = drain(h.handle_blocking(req("/?seed=1", "basic_tests/gc-protect-worker.php"))?);
    assert!(
        b1.contains("seeded"),
        "req1 seeds + bails in teardown (got {b1:?})"
    );
    let (_, b2) = drain(h.handle_blocking(req("/?probe=1", "basic_tests/gc-protect-worker.php"))?);
    assert!(
        b2.contains("unprotected"),
        "worker recovers from a teardown bailout with GC unprotected (got {b2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn error_get_last_cleared_between_worker_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("basic_tests/last-error-worker.php")))?;
    let h = r.handle();
    let (_, b1) =
        drain(h.handle_blocking(req("/?step=warn", "basic_tests/last-error-worker.php"))?);
    assert!(b1.contains("warned"), "req1 warns (got {b1:?})");
    let (_, b2) = drain(h.handle_blocking(req("/", "basic_tests/last-error-worker.php"))?);
    assert_eq!(
        b2, "clean",
        "error_get_last() must reset between jobs (got {b2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn first_call_teardown_bailout_recycles_instead_of_serving_on_corrupt_state() -> anyhow::Result<()>
{
    let _guard = php_lock();
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let sentinel = tmp.join(format!("rapira_h2_sentinel_{pid}"));
    let boot = tmp.join(format!("rapira_h2_boot_{pid}"));
    let _ = std::fs::remove_file(&sentinel);
    let _ = std::fs::remove_file(&boot);

    let r = Rapira::start(Mode::Worker(fixture("basic_tests/h2-boot-bail-worker.php")))?;
    let h = r.handle();
    let (_, body) = drain(h.handle_blocking(req("/", "basic_tests/h2-boot-bail-worker.php"))?);
    assert_eq!(
        body, "2",
        "first-call teardown bailout must recycle + re-bootstrap, not serve in cycle 1 (got {body:?})"
    );
    drop(h);
    r.shutdown();
    let _ = std::fs::remove_file(&sentinel);
    let _ = std::fs::remove_file(&boot);
    Ok(())
}

/// A post-loop warning left in PG(last_error_message) trips the core_globals_dtor assertion at php_module_shutdown (main.c:2102).
#[test]
fn worker_error_after_loop_exits_cleanly() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "basic_tests/warn-after-loop-worker.php",
    )))?;
    let h = r.handle();
    let (status, body) =
        drain(h.handle_blocking(req("/", "basic_tests/warn-after-loop-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "ok"));
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn filter_raw_input_does_not_accumulate() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("basic_tests/filter-leak-worker.php")))?;
    let h = r.handle();
    let mem = |b: String| -> i64 {
        b.trim()
            .strip_prefix("mem=")
            .and_then(|s| s.parse().ok())
            .expect("mem= output")
    };
    let (_, first) =
        drain(h.handle_blocking(req("/?x=warmup", "basic_tests/filter-leak-worker.php"))?);
    if first.trim() == "skip" {
        drop(h);
        r.shutdown();
        return Ok(());
    }
    for i in 0..5 {
        let _ = drain(h.handle_blocking(req(
            &format!("/?x=w{i}"),
            "basic_tests/filter-leak-worker.php",
        ))?);
    }
    let m1 =
        mem(drain(h.handle_blocking(req("/?x=base", "basic_tests/filter-leak-worker.php"))?).1);
    for i in 0..200 {
        let _ = drain(h.handle_blocking(req(
            &format!("/?x=v{i}"),
            "basic_tests/filter-leak-worker.php",
        ))?);
    }
    let m2 = mem(drain(h.handle_blocking(req("/?x=end", "basic_tests/filter-leak-worker.php"))?).1);
    let leaked = m2 - m1;
    assert!(
        leaked < 32 * 1024,
        "raw input copies must not accumulate across jobs; {leaked} bytes grown (~90KB pre-fix)"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn worker_finish_request_flush_bailout_recycles() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "basic_tests/finish-request-bailout-worker.php",
    )))?;
    let h = r.handle();
    let (s1, b1) = drain(h.handle_blocking(req(
        "/?boom=0",
        "basic_tests/finish-request-bailout-worker.php",
    ))?);
    let (s2, b2) = drain(h.handle_blocking(req(
        "/?boom=1",
        "basic_tests/finish-request-bailout-worker.php",
    ))?);
    let (s3, b3) = drain(h.handle_blocking(req(
        "/?boom=0",
        "basic_tests/finish-request-bailout-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 baseline (got: {b1:?})");
    assert_eq!(
        s2, 500,
        "fatal during the finish_request flush must commit a 500 (got {s2}, {b2:?})"
    );
    assert!(
        !b2.contains("resumed-after-fatal"),
        "script must not resume past the bailout (got: {b2:?})"
    );
    assert_eq!(s3, 200, "worker must recover (got {s3})");
    assert!(
        b3.contains("ok counter=1"),
        "recycle resets statics (got: {b3:?})"
    );
    Ok(())
}

/// Classic keeps early flush: pre-finish output ships, post-finish output is dropped, and the script keeps running.
#[test]
fn classic_finish_request() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "shared/finish-request-classic.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    assert!(
        body.contains("BEFORE") && !body.contains("AFTER"),
        "post-finish output must not reach the client (got: {body:?})"
    );
    let ran = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "post-finish-ran")
        .count();
    assert_eq!(ran, 1, "the script must keep running after the early flush");
    Ok(())
}
