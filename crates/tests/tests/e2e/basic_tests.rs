use http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use http::{Method, Version};
use rapira_sapi::plugin::Mode;
use std::net::SocketAddr;
use std::time::Duration;
use tests::wire::submit;
use tests::{StatusBody, drain, drain_resp, fixture, poll, req, server_log};

use crate::harness::{Server, Spawn, diagnostics, free_port, http_get, scratch_dir, slot_line};

#[test]
fn fibers_stress_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("basic_tests/fibers.php")).spawn();
    let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);

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
    let body1 = drain(submit(srv.addr, req("/?x=1"))?).body;
    let body2 = drain(submit(srv.addr, req("/?x=2"))?).body;
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
    let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);

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

/// The host releases a pending exception after the unit, and the destructor of that exception raises a fatal error. The bailout stays inside the C helper: the unit keeps its reply, the app instance recycles, and the same worker serves the next request.
#[test]
fn worker_survives_a_fatal_in_a_released_exception() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        query: &'static str,
        status: u16,
        /// The start of the body. display_errors appends the fatal text when the output is still open.
        body: &'static str,
    }
    let cases = [
        Case {
            name: "a shutdown function of the unit throws it",
            query: "/?boom=shutdown",
            // With display_errors on, php_error_cb keeps the 200 (main/main.c), and the flush commits that head.
            status: 200,
            body: "ok counter=2",
        },
        Case {
            name: "an output callback throws it at teardown, after the reply is sealed",
            query: "/?boom=teardown",
            status: 200,
            body: "buffered",
        },
    ];
    let srv = Spawn::http(Mode::Worker, fixture("shared/release-fatal-worker.php")).spawn();
    let baseline = drain(submit(srv.addr, req("/"))?);
    assert_eq!(
        (baseline.status, baseline.body.as_str()),
        (200, "ok counter=1")
    );

    for (i, case) in cases.iter().enumerate() {
        let StatusBody { status, body } = drain(submit(srv.addr, req(case.query))?);
        assert_eq!(status, case.status, "{}: status (body {body:?})", case.name);
        assert!(
            body.starts_with(case.body),
            "{}: want {:?} first, got {body:?}",
            case.name,
            case.body
        );

        let next = drain(submit(srv.addr, req("/"))?);
        assert_eq!(
            (next.status, next.body.as_str()),
            (200, baseline.body.as_str()),
            "{}: the same worker serves a new app instance",
            case.name
        );
        slot_line(&srv, &format!("recycles={} ", i + 1));
    }
    Ok(())
}

struct TeardownBailout {
    name: &'static str,
    query: &'static str,
    status: u16,
    body: &'static str,
    truncated: bool,
}

/// The seal runs inside rapira_request_teardown, so a bailout of the flush right before it must still reach the client.
#[test]
fn worker_survives_teardown_bailout() -> anyhow::Result<()> {
    let cases = [
        TeardownBailout {
            name: "buffered output: the seal commits a 500 head",
            query: "/?boom=1",
            status: 500,
            body: "",
            truncated: false,
        },
        TeardownBailout {
            name: "streamed output: the seal marks the cut",
            query: "/?boom=2",
            status: 200,
            body: "part",
            truncated: true,
        },
    ];
    let srv = Spawn::http(Mode::Worker, fixture("shared/teardown-bailout-worker.php")).spawn();
    let StatusBody {
        status: s1,
        body: b1,
    } = drain(submit(srv.addr, req("/?boom=0"))?);
    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "baseline (got: {b1:?})");

    for case in cases {
        let resp = drain_resp(submit(srv.addr, req(case.query))?);
        assert_eq!(
            resp.status(),
            case.status,
            "{}: status (body {:?})",
            case.name,
            resp.body_string()
        );
        assert_eq!(resp.body_string(), case.body, "{}: body", case.name);
        assert_eq!(resp.truncated, case.truncated, "{}: truncated", case.name);

        let StatusBody { status, body } = drain(submit(srv.addr, req("/?boom=0"))?);
        assert_eq!(
            status, 200,
            "{}: worker must recover after a teardown bailout",
            case.name
        );
        assert!(
            body.contains("ok counter=1"),
            "{}: recycle re-runs the bootstrap; statics reset (got: {body:?})",
            case.name
        );
    }
    Ok(())
}

/// A memory_limit fatal error ends the app instance with a heap that has no free slot for the next small allocation. The host reads the OPcache state at the app end, and a bailout of that read stays inside the C helper, so the same worker boots the app again and serves the next request.
#[test]
fn worker_survives_a_memory_limit_fatal_error() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/memory-limit-worker.php")).spawn();
    let before = drain(submit(srv.addr, req("/"))?);
    assert_eq!(before.status, 200, "before: {before:?}");
    drain(submit(srv.addr, req("/?leak=1"))?);
    let after = drain(submit(srv.addr, req("/"))?);
    assert_eq!(after.status, 200, "after the fatal error: {after:?}");
    assert_eq!(before.body.len(), 16, "boot token: {before:?}");
    assert_eq!(after.body.len(), 16, "boot token: {after:?}");
    assert_ne!(
        after.body,
        before.body,
        "the app booted again with a new boot id\n{}",
        diagnostics(&srv)
    );
    slot_line(&srv, "recycles=1 ");
    Ok(())
}

#[test]
fn worker_basic_auth() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/auth-worker.php")).spawn();

    let mut with_auth = req("/");
    with_auth.headers_mut().append(
        AUTHORIZATION,
        HeaderValue::from_static("Basic dXNlcjpwYXNz"),
    );
    let StatusBody {
        status: s_auth,
        body: b_auth,
    } = drain(submit(srv.addr, with_auth)?);

    let StatusBody {
        status: s_none,
        body: b_none,
    } = drain(submit(srv.addr, req("/"))?);

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
    *request.method_mut() = Method::POST;
    request
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    *request.body_mut() = b"foo".to_vec();
    request.headers_mut().append(
        AUTHORIZATION,
        HeaderValue::from_static("Basic dmFsZXJ5OnBhc3N3b3Jk"),
    );

    let StatusBody { status, body } = drain(submit(srv.addr, request)?);

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
    // The spawn stages the fixture as http/server-variables.php in its scratch dir; DOCUMENT_ROOT is the real path of the entrypoint directory.
    let root = std::fs::canonicalize(srv.dir.join("http"))?;
    for expected in [
        format!(
            "[SCRIPT_FILENAME] => {}\n",
            root.join("server-variables.php").display()
        ),
        format!("[DOCUMENT_ROOT] => {}\n", root.display()),
        "[SCRIPT_NAME] => /server-variables.php\n".into(),
        "[PHP_SELF] => /server-variables.php\n".into(),
        // https://www.rfc-editor.org/rfc/rfc3875#section-4.1.4
        "[GATEWAY_INTERFACE] => CGI/1.1\n".into(),
        // The listener has no TLS, and rapira runs no ident lookup.
        "[REQUEST_SCHEME] => http\n".into(),
        "[HTTPS] => \n".into(),
        "[REMOTE_IDENT] => \n".into(),
    ] {
        assert!(
            body.contains(&expected),
            "$_SERVER missing {expected:?} (got: {body:?})"
        );
    }
    Ok(())
}

/// SERVER_PROTOCOL follows the version of each request. One worker serves the cases in sequence.
#[test]
fn server_protocol_follows_the_request_version() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        version: Version,
        want: &'static str,
    }
    let cases = [
        Case {
            name: "an HTTP/1.1 request",
            version: Version::HTTP_11,
            want: "[SERVER_PROTOCOL] => HTTP/1.1\n",
        },
        Case {
            name: "an HTTP/1.0 request on the same worker",
            version: Version::HTTP_10,
            want: "[SERVER_PROTOCOL] => HTTP/1.0\n",
        },
        Case {
            name: "an HTTP/1.1 request after the HTTP/1.0 request",
            version: Version::HTTP_11,
            want: "[SERVER_PROTOCOL] => HTTP/1.1\n",
        },
    ];
    let srv = Spawn::http(Mode::Worker, fixture("shared/server-variables.php")).spawn();
    for case in &cases {
        let mut request = req("/");
        *request.version_mut() = case.version;
        let StatusBody { status, body } = drain(submit(srv.addr, request)?);
        assert_eq!(status, 200, "{}", case.name);
        assert!(
            body.contains(case.want),
            "{}: $_SERVER missing {:?} (got: {body:?})",
            case.name,
            case.want
        );
    }
    Ok(())
}

/// SCRIPT_NAME and PHP_SELF are URI paths, so an entrypoint that is a symlink to a file with another name keeps its configured name. SCRIPT_FILENAME is the real path.
#[test]
fn script_name_keeps_the_name_of_a_symlinked_entrypoint() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        mode: Mode,
        script: &'static str,
    }
    let cases = [
        Case {
            name: "classic mode",
            mode: Mode::Classic,
            script: "<?php\necho require __DIR__ . '/values.php';\n",
        },
        Case {
            name: "worker mode",
            mode: Mode::Worker,
            script: "<?php\n$handler = static function (): void {\n    echo require __DIR__ . '/values.php';\n};\nwhile (\\Rapira\\handle_request($handler)) {\n}\n",
        },
    ];
    for case in &cases {
        let dir = scratch_dir();
        std::fs::copy(fixture("boot_server/values.php"), dir.join("values.php"))?;
        std::fs::write(dir.join("front.php"), case.script)?;
        std::os::unix::fs::symlink("front.php", dir.join("index.php"))?;
        let srv =
            Spawn::http_entrypoint(case.mode, &dir.join("index.php").display().to_string()).spawn();

        let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);
        assert_eq!(status, 200, "{}: {body}", case.name);
        let got: serde_json::Value = serde_json::from_str(&body)?;
        let filename = std::fs::canonicalize(&dir)?.join("front.php");
        assert_eq!(got["SCRIPT_NAME"], "/index.php", "{}", case.name);
        assert_eq!(got["PHP_SELF"], "/index.php", "{}", case.name);
        assert_eq!(
            got["SCRIPT_FILENAME"],
            filename.display().to_string(),
            "{}",
            case.name
        );
        drop(srv);
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

#[test]
fn worker_finish_request() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/finish-request-worker.php")).spawn();

    let StatusBody {
        status: s1,
        body: b1,
    } = drain(submit(srv.addr, req("/"))?);
    let StatusBody {
        status: s2,
        body: b2,
    } = drain(submit(srv.addr, req("/"))?);

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
    let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn getenv_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/env-worker.php"))
        .env("FOO", "BAR")
        .spawn();
    let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn failboot_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("basic_tests/failboot.php")).spawn();
    let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 200);
    assert!(
        body.contains("syntax error, unexpected end of file, expecting variable or"),
        "expected error trace (got: {body:?})"
    );
    Ok(())
}

#[test]
fn scoreboard_counts_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/throw-worker.php")).spawn();
    let _ = drain(submit(srv.addr, req("/?boom=0"))?);
    let _ = drain(submit(srv.addr, req("/?boom=0"))?);
    let _ = drain(submit(srv.addr, req("/?boom=1"))?);
    slot_line(&srv, "handled=3 errors=1 ");
    Ok(())
}

#[test]
fn scoreboard_counts_recycles_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/shutdown-fatal-worker.php")).spawn();
    let _ = drain(submit(srv.addr, req("/?boom=1"))?);
    let s2 = drain(submit(srv.addr, req("/"))?).status;
    assert_eq!(s2, 200, "worker recovers after the recycle");
    let line = slot_line(&srv, "handled=2 ");
    assert!(
        line.contains("recycles=1 "),
        "the shutdown fatal recycles the app: {line}"
    );
    Ok(())
}

#[test]
fn scoreboard_counts_classic() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        script: &'static str,
        requests: usize,
        errors: usize,
    }
    let cases = [
        Case {
            name: "two clean requests",
            script: "shared/hello.php",
            requests: 2,
            errors: 0,
        },
        Case {
            name: "one compile error",
            script: "basic_tests/failboot.php",
            requests: 1,
            errors: 1,
        },
    ];
    for case in cases {
        let srv = Spawn::http(Mode::Classic, fixture(case.script)).spawn();
        for _ in 0..case.requests {
            let _ = drain(submit(srv.addr, req("/"))?);
        }
        let line = slot_line(&srv, &format!("handled={} ", case.requests));
        assert!(
            line.contains(&format!("errors={} ", case.errors)),
            "{}: {line}",
            case.name
        );
    }
    Ok(())
}

#[test]
fn worker_session_isolation() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/session-worker.php")).spawn();
    let StatusBody {
        status: s1,
        body: b1,
    } = drain(submit(srv.addr, req("/"))?);
    let StatusBody {
        status: s2,
        body: b2,
    } = drain(submit(srv.addr, req("/"))?);

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
    let status = drain(submit(srv.addr, req("/"))?).status;
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

fn php_levels(logged: &[server_log::Captured], mark: &str) -> Vec<tracing::Level> {
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
        vec![tracing::Level::WARN],
        "an unmasked E_USER_WARNING logs at warn (captured: {logged:?})"
    );
    assert_eq!(
        php_levels(&logged, "REPORTED-FATAL"),
        vec![tracing::Level::ERROR],
        "an uncaught throw stays an error-level diagnostic (captured: {logged:?})"
    );
    Ok(())
}

/// Fatal errors are exempt from the error_reporting(0) mask, because the recycle they cause still needs an explanation.
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

/// A throw that a shutdown function of the unit leaves is uncaught, as in the CLI. The exception handler of the app gets it, or it fails the unit as an uncaught throw from the handler does. The body began in the handler, so a failed unit ends cut.
#[test]
fn a_throw_from_a_shutdown_function_is_uncaught() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        step: &'static str,
        mark: &'static str,
        /// The start of the body. display_errors appends the fatal text.
        body: &'static str,
        truncated: bool,
        levels: Vec<tracing::Level>,
    }
    let cases = [
        Case {
            name: "no exception handler: an error record",
            step: "shutdown",
            mark: "Uncaught RuntimeException: SHUTDOWN-UNCAUGHT",
            body: "ok",
            truncated: true,
            levels: vec![tracing::Level::ERROR],
        },
        // This case is last, because its exception handler stays set for the later units of the app instance.
        Case {
            name: "the exception handler takes it: no record",
            step: "shutdown-handled",
            mark: "SHUTDOWN-HANDLED",
            body: "ok handled SHUTDOWN-HANDLED",
            truncated: false,
            levels: vec![],
        },
    ];
    let mut srv = error_levels_worker();
    let mut bodies = Vec::new();
    for case in &cases {
        let resp = drain_resp(submit(srv.addr, req(&format!("/?step={}", case.step)))?);
        let body = resp.body_string();
        assert_eq!(resp.status(), 200, "{}: status (body {body:?})", case.name);
        assert_eq!(resp.truncated, case.truncated, "{}: truncated", case.name);
        bodies.push(body);
    }
    srv.stop();

    let logged = server_log::records(&srv.log_file());
    for (case, body) in cases.iter().zip(bodies) {
        assert!(
            body.starts_with(case.body),
            "{}: want {:?} first, got {body:?}",
            case.name,
            case.body
        );
        assert_eq!(
            php_levels(&logged, case.mark),
            case.levels,
            "{} (captured: {logged:?})",
            case.name
        );
    }
    Ok(())
}

/// The teardown writes the session after the handler returns, and the save handler can throw there. The host sealed the response before that, so the throw gets the fatal record and fails the unit. The app exception handler does not run. The client gets the reply of php -S: status 200 and the whole body, streamed or buffered.
#[test]
fn a_throw_from_the_session_write_is_uncaught() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        /// php.ini lines after the shared file.
        ini: &'static str,
        /// The app exception handler that the fixture sets before the loop: "none" or "log".
        exception_handler: &'static str,
        uncaught: Vec<tracing::Level>,
        /// The contexts of the app records of the exception handler.
        handled: Vec<&'static str>,
        failed: u64,
    }
    let cases = [
        Case {
            name: "a streamed body and no exception handler: a fatal record and a failed unit",
            ini: "output_buffering = 0",
            exception_handler: "none",
            uncaught: vec![tracing::Level::ERROR],
            handled: vec![],
            failed: 1,
        },
        Case {
            name: "a buffered body and no exception handler: a fatal record and a failed unit",
            ini: "output_buffering = 4096",
            exception_handler: "none",
            uncaught: vec![tracing::Level::ERROR],
            handled: vec![],
            failed: 1,
        },
        Case {
            name: "an exception handler set before the loop does not run",
            ini: "output_buffering = 4096",
            exception_handler: "log",
            uncaught: vec![tracing::Level::ERROR],
            handled: vec![],
            failed: 1,
        },
    ];
    let shared = std::fs::read_to_string(fixture("ini/shared/php.ini"))?;
    for case in &cases {
        let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
        let mut srv = Spawn::http(
            Mode::Worker,
            fixture("basic_tests/session-write-throw-worker.php"),
        )
        .php_ini(&format!("{shared}\ndisplay_errors = Off\n{}\n", case.ini))
        .env("APP_EXCEPTION_HANDLER", case.exception_handler)
        .json_log()
        .toml(&format!(
            "[observability]\nlisten = \"{observability}\"\n[observability.metrics]"
        ))
        .spawn();
        let resp = drain_resp(submit(srv.addr, req("/"))?);
        // the slot counts the unit after its response goes out
        let failed = poll(Duration::from_secs(10), || {
            let body = http_get(observability, "/metrics", Duration::from_secs(10))
                .ok()?
                .body;
            let samples = tests::metrics::samples(&String::from_utf8_lossy(&body));
            if samples.get(r#"rapira_requests_total{pool="http"}"#) != Some(&1) {
                return None;
            }
            samples
                .get(r#"rapira_requests_failed_total{pool="http"}"#)
                .copied()
        });
        srv.stop();

        let body = resp.body_string();
        assert_eq!(resp.status(), 200, "{}: status (body {body:?})", case.name);
        assert_eq!(body, "session_status=1", "{}: body", case.name);
        assert!(resp.ended && !resp.truncated, "{}: truncated", case.name);
        let logged = server_log::records(&srv.log_file());
        assert_eq!(
            php_levels(&logged, "Uncaught RuntimeException: session write failed"),
            case.uncaught,
            "{} (captured: {logged:?})",
            case.name
        );
        assert_eq!(
            server_log::app_contexts(&srv.log_file(), "handled"),
            case.handled,
            "{}",
            case.name
        );
        assert_eq!(failed, Some(case.failed), "{}: failed units", case.name);
    }
    Ok(())
}

/// log_errors routes an unmasked deprecation through the SAPI log callback, which reports debug.
#[test]
fn logged_deprecation_stays_at_debug() -> anyhow::Result<()> {
    let mut srv = error_levels_worker();
    let status = drain(submit(srv.addr, req("/?step=logged"))?).status;
    srv.stop();

    assert_eq!(status, 200);
    let logged = server_log::records(&srv.log_file());
    assert_eq!(
        php_levels(&logged, "LOGGED-DEPRECATION"),
        vec![tracing::Level::DEBUG],
        "the log callback reports a deprecation at debug (captured: {logged:?})"
    );
    Ok(())
}

#[test]
fn sapi_ini_entries_applied() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("basic_tests/ini.php")).spawn();
    let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);

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
    let s1 = drain(submit(srv.addr, req("/?code=404"))?).status;
    let StatusBody {
        status: s2,
        body: b2,
    } = drain(submit(srv.addr, req("/"))?);

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
    let s1 = drain(submit(srv.addr, req("/?code=404"))?).status;
    let s2 = drain(submit(srv.addr, req("/"))?).status;

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
    let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 302);
    assert!(body.is_empty());
    Ok(())
}

#[test]
fn teardown_bailout_does_not_leave_gc_protected() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("basic_tests/gc-protect-worker.php")).spawn();
    let b1 = drain(submit(srv.addr, req("/?seed=1"))?).body;
    assert!(
        b1.contains("seeded"),
        "req1 seeds + bails in teardown (got {b1:?})"
    );
    let b2 = drain(submit(srv.addr, req("/?probe=1"))?).body;
    assert!(
        b2.contains("unprotected"),
        "worker recovers from a teardown bailout with GC unprotected (got {b2:?})"
    );
    Ok(())
}

#[test]
fn error_get_last_cleared_between_worker_requests() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("basic_tests/last-error-worker.php")).spawn();
    let b1 = drain(submit(srv.addr, req("/?step=warn"))?).body;
    assert!(b1.contains("warned"), "req1 warns (got {b1:?})");
    let b2 = drain(submit(srv.addr, req("/"))?).body;
    assert_eq!(
        b2, "clean",
        "error_get_last() must reset between units (got {b2:?})"
    );
    Ok(())
}

#[test]
fn first_call_teardown_bailout_recycles_instead_of_serving_on_corrupt_state() -> anyhow::Result<()>
{
    // The fixture keeps its cycle counter and sentinel in sys_get_temp_dir(), which is TMPDIR, an empty dir for each run.
    let tmp = scratch_dir();
    let srv = Spawn::http(Mode::Worker, fixture("basic_tests/h2-boot-bail-worker.php"))
        .env("TMPDIR", tmp.to_str().expect("utf-8 scratch dir"))
        .spawn();
    let body = drain(submit(srv.addr, req("/"))?).body;
    assert_eq!(
        body, "2",
        "first-call teardown bailout must recycle + re-bootstrap, not serve in cycle 1 (got {body:?})"
    );
    slot_line(&srv, "state=idle handled=1 errors=0 recycles=1 ");
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
    let first = drain(submit(srv.addr, req("/?x=warmup"))?).body;
    if first.trim() == "skip" {
        return Ok(());
    }
    for i in 0..5 {
        let _ = drain(submit(srv.addr, req(&format!("/?x=w{i}")))?);
    }
    let m1 = mem(drain(submit(srv.addr, req("/?x=base"))?).body);
    for i in 0..200 {
        let _ = drain(submit(srv.addr, req(&format!("/?x=v{i}")))?);
    }
    let m2 = mem(drain(submit(srv.addr, req("/?x=end"))?).body);
    let leaked = m2 - m1;
    assert!(
        leaked < 32 * 1024,
        "raw input copies must not accumulate across units; {leaked} bytes grown (~90KB pre-fix)"
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
    let StatusBody {
        status: s1,
        body: b1,
    } = drain(submit(srv.addr, req("/?boom=0"))?);
    let StatusBody {
        status: s2,
        body: b2,
    } = drain(submit(srv.addr, req("/?boom=1"))?);
    let StatusBody {
        status: s3,
        body: b3,
    } = drain(submit(srv.addr, req("/?boom=0"))?);

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

/// Classic mode keeps the early flush. The output before the finish is sent, the output after it is dropped, and the entrypoint keeps running.
#[test]
fn classic_finish_request() -> anyhow::Result<()> {
    let mut srv = Spawn::http(Mode::Classic, fixture("shared/finish-request-classic.php"))
        .json_log()
        .spawn();
    let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);
    // The SIGQUIT stop waits for the entrypoint, which keeps running after the early flush.
    srv.stop();

    assert_eq!(status, 200);
    assert!(
        body.contains("BEFORE") && !body.contains("AFTER"),
        "post-finish output must not reach the client (got: {body:?})"
    );
    let ran = server_log::app_contexts(&srv.log_file(), "post-finish-ran").len();
    assert_eq!(ran, 1, "the script must keep running after the early flush");
    Ok(())
}
