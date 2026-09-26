use std::time::{Duration, Instant};

use http::header::{AUTHORIZATION, HeaderValue};
use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{drain, fixture, req, server_log};

use crate::harness::{Server, Spawn, diagnostics, scratch_dir, signal};

#[test]
fn fibers_stress_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("basic_tests/fibers.php")).spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);

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
    let srv = Spawn::http(Mode::Worker, fixture("shared/leak-worker.php")).spawn();
    let (_, body1) = drain(submit(srv.addr, req("/?x=1"))?);
    let (_, body2) = drain(submit(srv.addr, req("/?x=2"))?);
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
    Ok(())
}

#[test]
fn fibers_stress_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/fibers-worker.php")).spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);

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
    let srv = Spawn::http(Mode::Worker, fixture("shared/teardown-bailout-worker.php")).spawn();
    let (s1, b1) = drain(submit(srv.addr, req("/?boom=0"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/?boom=1"))?);
    let (s3, b3) = drain(submit(srv.addr, req("/?boom=0"))?);

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
    Ok(())
}

#[test]
fn worker_basic_auth() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/auth-worker.php")).spawn();

    let mut with_auth = req("/");
    with_auth.headers.append(
        AUTHORIZATION,
        HeaderValue::from_static("Basic dXNlcjpwYXNz"),
    );
    let (s_auth, b_auth) = drain(submit(srv.addr, with_auth)?);

    let (s_none, b_none) = drain(submit(srv.addr, req("/"))?);

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
    let srv = Spawn::http(Mode::Worker, fixture("shared/server-variables.php"))
        .http_extra("server_port = 8080")
        .spawn();

    let mut request = req("/server-variables.php?foo=a&bar=b");
    request.method = "POST".into();
    request.content_type = Some("text/plain".into());
    request.body = rapira_sapi::types::Body::Raw(std::io::Cursor::new(b"foo".to_vec()));
    request.headers.append(
        AUTHORIZATION,
        HeaderValue::from_static("Basic dmFsZXJ5OnBhc3N3b3Jk"),
    );

    let (status, body) = drain(submit(srv.addr, request)?);

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
    // The spawn stages the fixture as http/server-variables.php in its scratch dir; DOCUMENT_ROOT is the directory of the entrypoint.
    let root = srv.dir.join("http");
    for expected in [
        format!(
            "[SCRIPT_FILENAME] => {}\n",
            root.join("server-variables.php").display()
        ),
        format!("[DOCUMENT_ROOT] => {}\n", root.display()),
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
    let srv = Spawn::http(Mode::Worker, fixture("shared/finish-request-worker.php")).spawn();

    let (s1, b1) = drain(submit(srv.addr, req("/"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/"))?);

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
    let srv = Spawn::http(Mode::Classic, fixture("basic_tests/env.php"))
        .env("FOO", "BAR")
        .spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn getenv_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/env-worker.php"))
        .env("FOO", "BAR")
        .spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn failboot_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("basic_tests/failboot.php")).spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 200);
    assert!(
        body.contains("syntax error, unexpected end of file, expecting variable or"),
        "expected error trace (got: {body:?})"
    );
    Ok(())
}

/// The counters of the one worker slot.
#[derive(Debug)]
struct Slot {
    handled: u64,
    errors: u64,
    recycles: u64,
}

/// On SIGUSR1 the master logs one `slot {id} pid {pid} state {state} handled {n} errors {n} recycles {n}` line per slot; the server needs `.json_log()`.
fn slot(srv: &Server) -> Slot {
    signal(srv.pid(), libc::SIGUSR1);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut slots: Vec<Slot> = server_log::records(&srv.log_file())
            .iter()
            .filter(|c| c.target == "master")
            .filter_map(|c| slot_line(&c.message))
            .collect();
        match slots.len() {
            1 => return slots.remove(0),
            0 if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => panic!(
                "expected one slot line, got {slots:?}\n{}",
                diagnostics(srv)
            ),
        }
    }
}

fn slot_line(message: &str) -> Option<Slot> {
    let rest = message.trim().strip_prefix("slot ")?;
    let count = |name: &str| -> Option<u64> {
        let mut words = rest.split_whitespace();
        words.find(|w| *w == name)?;
        words.next()?.parse().ok()
    };
    Some(Slot {
        handled: count("handled")?,
        errors: count("errors")?,
        recycles: count("recycles")?,
    })
}

#[test]
fn scoreboard_counts_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/throw-worker.php"))
        .json_log()
        .spawn();
    let _ = drain(submit(srv.addr, req("/?boom=0"))?);
    let _ = drain(submit(srv.addr, req("/?boom=0"))?);
    let _ = drain(submit(srv.addr, req("/?boom=1"))?);
    let snap = slot(&srv);

    assert_eq!(snap.handled, 3, "3 requests handled");
    assert_eq!(snap.errors, 1, "one engine error (uncaught throw)");
    Ok(())
}

#[test]
fn scoreboard_counts_recycles_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/shutdown-fatal-worker.php"))
        .json_log()
        .spawn();
    let _ = drain(submit(srv.addr, req("/?boom=1"))?);
    let (s2, _) = drain(submit(srv.addr, req("/"))?);
    let snap = slot(&srv);

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
    // A classic worker runs one entrypoint, so each script gets its own worker: (handled, errors).
    let counts = |script: &str, requests: usize| -> anyhow::Result<(u64, u64)> {
        let srv = Spawn::http(Mode::Classic, fixture(script))
            .json_log()
            .spawn();
        for _ in 0..requests {
            let _ = drain(submit(srv.addr, req("/"))?);
        }
        let snap = slot(&srv);
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
    let srv = Spawn::http(Mode::Worker, fixture("shared/session-worker.php")).spawn();
    let (s1, b1) = drain(submit(srv.addr, req("/"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/"))?);

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
    let mut srv = Spawn::http(Mode::Worker, fixture("basic_tests/boot-output-worker.php"))
        .json_log()
        .spawn();
    let (status, _) = drain(submit(srv.addr, req("/"))?);
    srv.stop();

    assert_eq!(status, 200, "worker still serves after no-context output");
    let logged = server_log::records(&srv.log_file());
    assert!(
        logged
            .iter()
            .any(|c| c.message.contains("WORKER-BOOT-OUTPUT")),
        "worker bootstrap output must be logged (captured: {logged:?})"
    );
    Ok(())
}

/// The php target logs a masked diagnostic at trace and a deprecation at debug, so the filter lets every php record through.
fn error_levels_worker() -> Server {
    Spawn::http(Mode::Worker, fixture("basic_tests/error-levels-worker.php"))
        .json_log()
        .rust_log("info,php=trace")
        .spawn()
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
    let mut srv = error_levels_worker();
    for step in ["deprecated", "warn", "boom"] {
        let uri = format!("/?step={step}");
        let _ = drain(submit(srv.addr, req(&uri))?);
    }
    srv.stop();

    let logged = server_log::records(&srv.log_file());
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
    let mut srv = error_levels_worker();
    let _ = drain(submit(srv.addr, req("/?step=silent-fatal"))?);
    srv.stop();

    let logged = server_log::records(&srv.log_file());
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
    let mut srv = error_levels_worker();
    let (status, _) = drain(submit(srv.addr, req("/?step=logged"))?);
    srv.stop();

    assert_eq!(status, 200);
    let logged = server_log::records(&srv.log_file());
    assert_eq!(
        php_levels(&logged, "LOGGED-DEPRECATION"),
        vec![tracing::Level::DEBUG, tracing::Level::DEBUG],
        "the log callback reports a deprecation at debug (captured: {logged:?})"
    );
    Ok(())
}

#[test]
fn sapi_ini_entries_applied() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("basic_tests/ini.php")).spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);

    assert_eq!(status, 200);
    assert!(
        body.contains("met=0"),
        "ini_entries must apply: max_execution_time=0 (got: {body:?})"
    );
    Ok(())
}

#[test]
fn status_code_does_not_leak_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("basic_tests/status-worker.php")).spawn();
    let (s1, _) = drain(submit(srv.addr, req("/?code=404"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/"))?);

    assert_eq!(s1, 404, "explicit http_response_code(404)");
    assert_eq!(
        s2, 200,
        "default status must be 200, not the leaked 404 (body: {b2:?})"
    );
    Ok(())
}

#[test]
fn status_code_does_not_leak_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("basic_tests/status.php")).spawn();
    let (s1, _) = drain(submit(srv.addr, req("/?code=404"))?);
    let (s2, _) = drain(submit(srv.addr, req("/"))?);

    assert_eq!(s1, 404);
    assert_eq!(
        s2, 200,
        "classic mode reuses SG on the thread; 404 must not leak"
    );
    Ok(())
}

#[test]
fn worker_finish_request_header_only() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("basic_tests/finish-request-headers-worker.php"),
    )
    .spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 302);
    assert!(body.is_empty());
    Ok(())
}

#[test]
fn teardown_bailout_does_not_leave_gc_protected() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("basic_tests/gc-protect-worker.php")).spawn();
    let (_, b1) = drain(submit(srv.addr, req("/?seed=1"))?);
    assert!(
        b1.contains("seeded"),
        "req1 seeds + bails in teardown (got {b1:?})"
    );
    let (_, b2) = drain(submit(srv.addr, req("/?probe=1"))?);
    assert!(
        b2.contains("unprotected"),
        "worker recovers from a teardown bailout with GC unprotected (got {b2:?})"
    );
    Ok(())
}

#[test]
fn error_get_last_cleared_between_worker_requests() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("basic_tests/last-error-worker.php")).spawn();
    let (_, b1) = drain(submit(srv.addr, req("/?step=warn"))?);
    assert!(b1.contains("warned"), "req1 warns (got {b1:?})");
    let (_, b2) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(
        b2, "clean",
        "error_get_last() must reset between jobs (got {b2:?})"
    );
    Ok(())
}

#[test]
fn first_call_teardown_bailout_recycles_instead_of_serving_on_corrupt_state() -> anyhow::Result<()>
{
    // The fixture keeps its cycle counter and sentinel in sys_get_temp_dir(), which is TMPDIR: an empty dir per run.
    let tmp = scratch_dir();
    let srv = Spawn::http(Mode::Worker, fixture("basic_tests/h2-boot-bail-worker.php"))
        .env("TMPDIR", tmp.to_str().expect("utf-8 scratch dir"))
        .spawn();
    let (_, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(
        body, "2",
        "first-call teardown bailout must recycle + re-bootstrap, not serve in cycle 1 (got {body:?})"
    );
    std::fs::remove_dir_all(&tmp)?;
    Ok(())
}

#[test]
fn filter_raw_input_does_not_accumulate() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("basic_tests/filter-leak-worker.php")).spawn();
    let mem = |b: String| -> i64 {
        b.trim()
            .strip_prefix("mem=")
            .and_then(|s| s.parse().ok())
            .expect("mem= output")
    };
    let (_, first) = drain(submit(srv.addr, req("/?x=warmup"))?);
    if first.trim() == "skip" {
        return Ok(());
    }
    for i in 0..5 {
        let _ = drain(submit(srv.addr, req(&format!("/?x=w{i}")))?);
    }
    let m1 = mem(drain(submit(srv.addr, req("/?x=base"))?).1);
    for i in 0..200 {
        let _ = drain(submit(srv.addr, req(&format!("/?x=v{i}")))?);
    }
    let m2 = mem(drain(submit(srv.addr, req("/?x=end"))?).1);
    let leaked = m2 - m1;
    assert!(
        leaked < 32 * 1024,
        "raw input copies must not accumulate across jobs; {leaked} bytes grown (~90KB pre-fix)"
    );
    Ok(())
}

#[test]
fn worker_finish_request_flush_bailout_recycles() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("basic_tests/finish-request-bailout-worker.php"),
    )
    .spawn();
    let (s1, b1) = drain(submit(srv.addr, req("/?boom=0"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/?boom=1"))?);
    let (s3, b3) = drain(submit(srv.addr, req("/?boom=0"))?);

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
    let mut srv = Spawn::http(Mode::Classic, fixture("shared/finish-request-classic.php"))
        .json_log()
        .spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);
    // The graceful stop waits for the script, which keeps running after the early flush.
    srv.stop();

    assert_eq!(status, 200);
    assert!(
        body.contains("BEFORE") && !body.contains("AFTER"),
        "post-finish output must not reach the client (got: {body:?})"
    );
    let ran = server_log::records(&srv.log_file())
        .iter()
        .filter(|c| c.target == "app" && c.message == "post-finish-ran")
        .count();
    assert_eq!(ran, 1, "the script must keep running after the early flush");
    Ok(())
}
