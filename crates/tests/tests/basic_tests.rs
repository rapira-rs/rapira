use http::header::{AUTHORIZATION, HeaderValue};
use rapira_sapi::{Mode, Rapira};
use tests::{captured, drain, fixture, init_log_capture, php_lock, req};

#[test]
fn fibers_stress_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic, fixture("basic_tests/fibers.php"), None)?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);

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
    let r = Rapira::start(Mode::Worker, fixture("shared/leak-worker.php"), None)?;
    let h = r.sink();
    let (_, body1) = drain(tests::submit(&h, req("/?x=1"))?);
    let (_, body2) = drain(tests::submit(&h, req("/?x=2"))?);
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
    drop(r);
    Ok(())
}

#[test]
fn fibers_stress_worker() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker, fixture("shared/fibers-worker.php"), None)?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);

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

    let r = Rapira::start(
        Mode::Worker,
        fixture("shared/teardown-bailout-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (s1, b1) = drain(tests::submit(&h, req("/?boom=0"))?);
    let (s2, b2) = drain(tests::submit(&h, req("/?boom=1"))?);
    let (s3, b3) = drain(tests::submit(&h, req("/?boom=0"))?);

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
    drop(r);
    Ok(())
}

#[test]
fn worker_basic_auth() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker, fixture("shared/auth-worker.php"), None)?;
    let h = r.sink();

    let mut with_auth = req("/");
    with_auth.headers.append(
        AUTHORIZATION,
        HeaderValue::from_static("Basic dXNlcjpwYXNz"),
    );
    let (s_auth, b_auth) = drain(tests::submit(&h, with_auth)?);

    let (s_none, b_none) = drain(tests::submit(&h, req("/"))?);

    drop(h);
    drop(r);

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

    let r: Rapira = Rapira::start(Mode::Worker, fixture("shared/server-variables.php"), None)?;
    let h: rapira_sapi::work::Sink = r.sink();

    let mut request: rapira_sapi::Request = req("/server-variables.php?foo=a&bar=b");
    request.method = "POST".into();
    request.content_type = Some("text/plain".into());
    request.content_length = 3;
    request.body = rapira_sapi::types::Body::Raw(std::io::Cursor::new(b"foo".to_vec()));
    request.headers.append(
        AUTHORIZATION,
        HeaderValue::from_static("Basic dmFsZXJ5OnBhc3N3b3Jk"),
    );

    let (status, body) = drain(tests::submit(&h, request)?);
    drop(h);
    drop(r);

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

    let r = Rapira::start(
        Mode::Worker,
        fixture("shared/finish-request-worker.php"),
        None,
    )?;
    let h = r.sink();

    let (s1, b1) = drain(tests::submit(&h, req("/"))?);
    let (s2, b2) = drain(tests::submit(&h, req("/"))?);

    drop(h);
    drop(r);

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
    let r = Rapira::start(Mode::Classic, fixture("basic_tests/env.php"), None)?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);
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
    let r = Rapira::start(Mode::Worker, fixture("shared/env-worker.php"), None)?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn failboot_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic, fixture("basic_tests/failboot.php"), None)?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);
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
    let r = Rapira::start(Mode::Worker, fixture("shared/throw-worker.php"), None)?;
    let h = r.sink();
    let _ = drain(tests::submit(&h, req("/?boom=0"))?);
    let _ = drain(tests::submit(&h, req("/?boom=0"))?);
    let _ = drain(tests::submit(&h, req("/?boom=1"))?);
    drop(h);
    let snap = r.scoreboard().expect("private scoreboard slot");
    drop(r);

    assert_eq!(snap.handled, 3, "3 requests handled");
    assert_eq!(snap.errors, 1, "one engine error (uncaught throw)");
    Ok(())
}

#[test]
fn scoreboard_counts_recycles_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        Mode::Worker,
        fixture("shared/shutdown-fatal-worker.php"),
        None,
    )?;
    let h = r.sink();
    let _ = drain(tests::submit(&h, req("/?boom=1"))?);
    let (s2, _) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    let snap = r.scoreboard().expect("private scoreboard slot");
    drop(r);

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
    // A classic worker runs one entrypoint, so each script gets its own worker: (handled, errors).
    let counts = |script: &str, requests: usize| -> anyhow::Result<(u64, u64)> {
        let r = Rapira::start(Mode::Classic, fixture(script), None)?;
        let h = r.sink();
        for _ in 0..requests {
            let _ = drain(tests::submit(&h, req("/"))?);
        }
        drop(h);
        let snap = r.scoreboard().expect("private scoreboard slot");
        drop(r);
        Ok((snap.handled, snap.errors))
    };

    assert_eq!(counts("shared/hello.php", 2)?, (2, 0), "two clean requests");
    assert_eq!(
        counts("basic_tests/failboot.php", 1)?,
        (1, 1),
        "one PHP error (failboot.php fails to compile)"
    );
    Ok(())
}

#[test]
fn worker_session_isolation() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker, fixture("shared/session-worker.php"), None)?;
    let h = r.sink();
    let (s1, b1) = drain(tests::submit(&h, req("/"))?);
    let (s2, b2) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);

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

    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/boot-output-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (status, _) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);

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

    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/error-levels-worker.php"),
        None,
    )?;
    let h = r.sink();
    for step in ["deprecated", "warn", "boom"] {
        let uri = format!("/?step={step}");
        let _ = drain(tests::submit(&h, req(&uri))?);
    }
    drop(h);
    drop(r);

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

    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/error-levels-worker.php"),
        None,
    )?;
    let h = r.sink();
    let _ = drain(tests::submit(&h, req("/?step=silent-fatal"))?);
    drop(h);
    drop(r);

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

    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/error-levels-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (status, _) = drain(tests::submit(&h, req("/?step=logged"))?);
    drop(h);
    drop(r);

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
    let r = Rapira::start(Mode::Classic, fixture("basic_tests/ini.php"), None)?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);

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
    let r = Rapira::start(Mode::Worker, fixture("basic_tests/status-worker.php"), None)?;
    let h = r.sink();
    let (s1, _) = drain(tests::submit(&h, req("/?code=404"))?);
    let (s2, b2) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);

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
    let r = Rapira::start(Mode::Classic, fixture("basic_tests/status.php"), None)?;
    let h = r.sink();
    let (s1, _) = drain(tests::submit(&h, req("/?code=404"))?);
    let (s2, _) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);

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
    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/finish-request-headers-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);
    assert_eq!(status, 302);
    assert!(body.is_empty());
    Ok(())
}

#[test]
fn teardown_bailout_does_not_leave_gc_protected() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/gc-protect-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (_, b1) = drain(tests::submit(&h, req("/?seed=1"))?);
    assert!(
        b1.contains("seeded"),
        "req1 seeds + bails in teardown (got {b1:?})"
    );
    let (_, b2) = drain(tests::submit(&h, req("/?probe=1"))?);
    assert!(
        b2.contains("unprotected"),
        "worker recovers from a teardown bailout with GC unprotected (got {b2:?})"
    );
    drop(h);
    drop(r);
    Ok(())
}

#[test]
fn error_get_last_cleared_between_worker_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/last-error-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (_, b1) = drain(tests::submit(&h, req("/?step=warn"))?);
    assert!(b1.contains("warned"), "req1 warns (got {b1:?})");
    let (_, b2) = drain(tests::submit(&h, req("/"))?);
    assert_eq!(
        b2, "clean",
        "error_get_last() must reset between jobs (got {b2:?})"
    );
    drop(h);
    drop(r);
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

    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/h2-boot-bail-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (_, body) = drain(tests::submit(&h, req("/"))?);
    assert_eq!(
        body, "2",
        "first-call teardown bailout must recycle + re-bootstrap, not serve in cycle 1 (got {body:?})"
    );
    drop(h);
    drop(r);
    let _ = std::fs::remove_file(&sentinel);
    let _ = std::fs::remove_file(&boot);
    Ok(())
}

/// A post-loop warning left in PG(last_error_message) trips the core_globals_dtor assertion at php_module_shutdown (main.c:2102).
#[test]
fn worker_error_after_loop_exits_cleanly() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/warn-after-loop-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    assert_eq!((status, body.as_str()), (200, "ok"));
    drop(h);
    drop(r);
    Ok(())
}

#[test]
fn filter_raw_input_does_not_accumulate() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/filter-leak-worker.php"),
        None,
    )?;
    let h = r.sink();
    let mem = |b: String| -> i64 {
        b.trim()
            .strip_prefix("mem=")
            .and_then(|s| s.parse().ok())
            .expect("mem= output")
    };
    let (_, first) = drain(tests::submit(&h, req("/?x=warmup"))?);
    if first.trim() == "skip" {
        drop(h);
        drop(r);
        return Ok(());
    }
    for i in 0..5 {
        let _ = drain(tests::submit(&h, req(&format!("/?x=w{i}")))?);
    }
    let m1 = mem(drain(tests::submit(&h, req("/?x=base"))?).1);
    for i in 0..200 {
        let _ = drain(tests::submit(&h, req(&format!("/?x=v{i}")))?);
    }
    let m2 = mem(drain(tests::submit(&h, req("/?x=end"))?).1);
    let leaked = m2 - m1;
    assert!(
        leaked < 32 * 1024,
        "raw input copies must not accumulate across jobs; {leaked} bytes grown (~90KB pre-fix)"
    );
    drop(h);
    drop(r);
    Ok(())
}

#[test]
fn worker_finish_request_flush_bailout_recycles() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        Mode::Worker,
        fixture("basic_tests/finish-request-bailout-worker.php"),
        None,
    )?;
    let h = r.sink();
    let (s1, b1) = drain(tests::submit(&h, req("/?boom=0"))?);
    let (s2, b2) = drain(tests::submit(&h, req("/?boom=1"))?);
    let (s3, b3) = drain(tests::submit(&h, req("/?boom=0"))?);
    drop(h);
    drop(r);

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

    let r = Rapira::start(
        Mode::Classic,
        fixture("shared/finish-request-classic.php"),
        None,
    )?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/"))?);
    drop(h);
    drop(r);

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
