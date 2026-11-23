use crate::harness::*;
use rapira_config::Mode;
use std::time::{Duration, Instant};
use tests::wire::submit;
use tests::{StatusBody, drain_resp, poll, req, server_log};

#[test]
#[cfg_attr(php_zts, ignore = "needs NTS PHP")]
fn nts_uses_one_worker_and_warns_when_four_are_configured() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path("shared/echo-worker.php"))
        .workers(4)
        .json_log()
        .spawn();
    wait_serving(&srv, "http", 1);
    pool_line(&srv, "http", "workers=1 ");
    assert!(
        server_log::records(&srv.log_file()).iter().any(|record| {
            record.level == tracing::Level::WARN && record.message.contains("--enable-zts")
        }),
        "no NTS worker limit warning\n{}",
        diagnostics(&srv)
    );
    assert_eq!(http_get(srv.addr, "/", BOOT).expect("GET /").status, 200);
}

#[test]
#[cfg_attr(php_zts, ignore = "needs NTS PHP")]
fn nts_refuses_two_pools() {
    let preload = fixture_path("lifecycle/preload-marker.php");
    let mut ini = format!(
        "opcache.enable = 1\nopcache.preload = \"{}\"\n",
        preload.display()
    );
    if unsafe { libc::geteuid() } == 0 {
        ini.push_str("opcache.preload_user = root\n");
    }
    let Exited { status, log } =
        Spawn::http(Mode::Dispatcher, fixture_path("shared/echo-worker.php"))
            .with_grpc(fixture_path("grpc/echo-worker.php"))
            .php_ini(&ini)
            .boot_failure();
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(log.contains("NTS PHP supports one pool"), "{log}");
    assert!(!log.contains("preload ran"), "{log}");
}

#[test]
fn usr1_reports_the_current_worker_counters() {
    struct Case {
        name: &'static str,
        mode: Mode,
        fixture: &'static str,
    }
    let cases = [
        Case {
            name: "classic",
            mode: Mode::Classic,
            fixture: "classic/cwd.php",
        },
        Case {
            name: "worker",
            mode: Mode::Worker,
            fixture: "lifecycle/boot-log-handle-worker.php",
        },
    ];
    for case in cases {
        let srv = Spawn::http(case.mode, fixture_path(case.fixture)).spawn();
        wait_serving(&srv, "http", 1);
        let before = slot_line(&srv, "state=idle ");
        assert!(
            before.contains("handled=0 errors=0 "),
            "{}: {before}",
            case.name
        );
        assert_eq!(http_get(srv.addr, "/", BOOT).expect("GET /").status, 200);
        let after = slot_line(&srv, "state=idle ");
        assert!(
            after.contains("handled=1 errors=0 "),
            "{}: {after}",
            case.name
        );
    }
}

#[test]
fn retained_spl_tempfile_survives_next_request() {
    let srv = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/retained-temp-worker.php"),
    )
    .spawn();
    let timeout = Duration::from_secs(10);
    wait_serving(&srv, "http", 1);

    for (index, contents) in ["first export\n", "replacement export\n"]
        .into_iter()
        .enumerate()
    {
        let StatusBody { status: code, body } = http_post(
            srv.addr,
            "/export",
            b"text/plain",
            contents.as_bytes(),
            timeout,
        )
        .unwrap_or_else(|e| panic!("POST /export: {e}\n{}", diagnostics(&srv)));
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
        assert_eq!(
            body,
            format!("{}:stored", index * 2 + 1).into_bytes(),
            "\n{}",
            diagnostics(&srv)
        );

        let StatusBody { status: code, body } = http_get(srv.addr, "/export", timeout)
            .unwrap_or_else(|e| panic!("GET /export: {e}\n{}", diagnostics(&srv)));
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
        assert_eq!(
            body,
            format!("{}:{contents}", index * 2 + 2).into_bytes(),
            "\n{}",
            diagnostics(&srv)
        );
    }
}

#[test]
fn an_idle_server_stops_on_quit_term_or_int() {
    struct Case {
        name: &'static str,
        signal: i32,
    }
    let cases = [
        Case {
            name: "QUIT",
            signal: libc::SIGQUIT,
        },
        Case {
            name: "TERM",
            signal: libc::SIGTERM,
        },
        Case {
            name: "INT",
            signal: libc::SIGINT,
        },
    ];
    for case in cases {
        let mut srv = Spawn::http(Mode::Classic, fixture_path("classic/cwd.php")).spawn();
        wait_serving(&srv, "http", 1);
        assert_eq!(http_get(srv.addr, "/", BOOT).expect(case.name).status, 200);
        signal(srv.pid(), case.signal);
        assert_exit_code(srv.wait_exit(STOP_BUDGET), EXIT_OK, &srv);
    }
}

/// The held request keeps the stop open. The pidfile names the server until it exits.
#[test]
fn the_pidfile_names_the_server_until_its_exit() {
    enum Stop {
        Release,
        Deadline,
        SecondTerm,
    }
    struct Case {
        name: &'static str,
        stop: Stop,
        timeout_secs: u64,
        want_exit: i32,
    }
    let cases = [
        Case {
            name: "the held request completes",
            stop: Stop::Release,
            timeout_secs: 30,
            want_exit: EXIT_OK,
        },
        Case {
            name: "the held request exceeds the stop deadline",
            stop: Stop::Deadline,
            timeout_secs: 2,
            want_exit: EXIT_OK,
        },
        Case {
            name: "a second TERM during the stop",
            stop: Stop::SecondTerm,
            timeout_secs: 30,
            want_exit: 130,
        },
    ];
    for case in cases {
        let mut srv = Spawn::http(Mode::Worker, tests::fixture("worker/held-worker.php"))
            .toml(&format!(
                "[server]\npidfile = \"rapira.pid\"\nstop_timeout_secs = {}",
                case.timeout_secs
            ))
            .json_log()
            .spawn();
        let pidfile = srv.dir.join("rapira.pid");
        let release = srv.dir.join("release");
        wait_serving(&srv, "http", 1);
        let mut held = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
        held.send(
            format!(
                "GET /?release={} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
                release.display()
            )
            .as_bytes(),
        )
        .expect("send the held request");
        tests::server_log::wait_app_record(&srv.log_file(), "held");

        let start = Instant::now();
        signal(srv.pid(), libc::SIGTERM);
        // Two pending TERM signals can merge. Wait for the server to take the first one.
        assert!(
            wait_log_contains(&srv, "stopping server", Duration::from_secs(10)),
            "{}: no drain\n{}",
            case.name,
            diagnostics(&srv)
        );
        let content = std::fs::read_to_string(&pidfile).expect("read the pidfile during the stop");
        assert_eq!(
            content.trim(),
            srv.pid().to_string(),
            "{}: the pidfile during the stop",
            case.name
        );
        match case.stop {
            Stop::Release => {
                std::fs::write(&release, "").expect("release the request");
                let raw = held.read_remaining(BOOT).expect("read the held response");
                let StatusBody { status, body } = parse_status_and_body(&raw).expect("response");
                assert_eq!((status, body), (200, b"done".as_slice()), "{}", case.name);
            }
            Stop::SecondTerm => signal(srv.pid(), libc::SIGTERM),
            Stop::Deadline => {}
        }
        // The wait covers the 2 s deadline. The other cases must exit before their 30 s deadline.
        let status = srv.wait_exit(Duration::from_secs(10));
        assert_exit_code(status, case.want_exit, &srv);
        if matches!(case.stop, Stop::Deadline) {
            assert!(
                start.elapsed() >= Duration::from_secs(case.timeout_secs),
                "the server stopped before its deadline"
            );
        }
        assert!(
            !pidfile.exists(),
            "{}: the pidfile stays after the exit",
            case.name
        );
    }
}

/// max_requests = 2 is a quota of 3. A served unit uses the quota, but a queued unit whose client left does not. After A and two such units, the count probe still reaches the app instance that served A.
#[test]
fn queued_clients_gone_do_not_use_the_max_requests_quota() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        mode: Mode,
        fixture: &'static str,
    }
    let cases = [
        Case {
            name: "dispatcher mode",
            mode: Mode::Dispatcher,
            fixture: "dispatcher/held-worker.php",
        },
        Case {
            name: "worker mode",
            mode: Mode::Worker,
            fixture: "worker/held-worker.php",
        },
    ];
    for case in cases {
        let srv = Spawn::http(case.mode, tests::fixture(case.fixture))
            .http_pool("max_requests = 2")
            .json_log()
            .spawn();
        let release = srv.log_file().with_file_name("release");
        let path = format!("/?release={}", release.display());

        let rx_a = submit(srv.addr, req(&path))?;
        server_log::wait_app_record(&srv.log_file(), "held");
        let rx_b = submit(srv.addr, req(&path))?;
        let rx_c = submit(srv.addr, req(&path))?;
        pool_line(&srv, "http", "pending=2 ");
        drop((rx_b, rx_c));
        std::thread::sleep(Duration::from_secs(1));
        std::fs::write(&release, "")?;
        assert_eq!(drain_resp(rx_a).body_string(), "done", "{}", case.name);

        let resp = drain_resp(submit(srv.addr, req("/?probe=count"))?);
        assert_eq!(
            resp.body_string(),
            "runs=1",
            "{}: the app instance that served A answers\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
    Ok(())
}

/// max_requests = 2 is a quota of 3, so the storm recycles the app many times. Each recycle boots the app again in the same process, and no request fails.
#[test]
fn max_requests_restarts_the_app_without_losing_requests() {
    struct Case {
        name: &'static str,
        mode: Mode,
        fixture: &'static str,
    }
    let cases = [
        Case {
            name: "dispatcher mode",
            mode: Mode::Dispatcher,
            fixture: "lifecycle/boot-log-worker.php",
        },
        Case {
            name: "worker mode",
            mode: Mode::Worker,
            fixture: "lifecycle/boot-log-handle-worker.php",
        },
    ];
    for case in cases {
        let srv = Spawn::http(case.mode, fixture_path(case.fixture))
            .http_pool("max_requests = 2")
            .json_log()
            .spawn();
        wait_serving(&srv, "http", 1);
        let storm = storm(srv.addr, 8);
        std::thread::sleep(Duration::from_secs(2));
        let tally = storm.halt();
        assert_eq!(
            (tally.failed, tally.refused, tally.dropped),
            (0, 0, 0),
            "{}: failed, refused and dropped requests; last error: {:?}\n{}",
            case.name,
            tally.last_err,
            diagnostics(&srv)
        );
        assert!(tally.ok > 0, "{}: the storm got no response", case.name);
        let log = srv.log_file();
        assert!(
            tests::server_log::app_contexts(&log, "booted").len() >= 2,
            "{}: the app booted again\n{}",
            case.name,
            diagnostics(&srv)
        );
        // In worker mode, handle_request() returned false, so the code after the loop ran.
        assert!(
            !tests::server_log::app_contexts(&log, "app loop ended").is_empty(),
            "{}: the app left its loop\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
}

/// A top-level `[pool]` table is a parse error, because the pool belongs under `[http.pool]`.
#[test]
fn a_top_level_pool_table_refuses_to_boot() {
    let Exited { status, log } =
        Spawn::http(Mode::Dispatcher, fixture_path("shared/echo-worker.php"))
            .http_extra("[pool]\nworkers = 1")
            .boot_failure();
    assert_eq!(status.code(), Some(1), "\n{log}");
    assert!(log.contains("parsing config file"), "\n{log}");
    assert!(log.contains("unknown field `pool`"), "\n{log}");
}

/// The server checks at boot that the entrypoint is readable.
#[test]
fn a_missing_entrypoint_refuses_to_boot() {
    let Exited { status, log } =
        Spawn::http_entrypoint(Mode::Dispatcher, "no-such-entrypoint.php").boot_failure();
    assert_eq!(status.code(), Some(1), "\n{log}");
    assert!(log.contains("http.pool.entrypoint"), "\n{log}");
    assert!(log.contains("no-such-entrypoint.php"), "\n{log}");
    assert!(log.contains("is not readable"), "\n{log}");
}

#[test]
fn an_unreadable_entrypoint_refuses_to_boot() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir();
    let script = dir.join("index.php");
    std::fs::write(&script, "<?php\n").expect("write entrypoint");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    // Root bypasses permission checks; the case cannot occur for that user.
    if std::fs::read(&script).is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let Exited { status, log } =
        Spawn::http_entrypoint(Mode::Dispatcher, &script.display().to_string()).boot_failure();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "\n{log}");
    assert!(log.contains("http.pool.entrypoint"), "\n{log}");
    assert!(log.contains("is not readable"), "\n{log}");
}

/// One worker waits at boot. The other reaches the limit before the pool can serve.
#[test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
fn a_boot_failure_limit_in_one_worker_stops_a_pool_that_never_served() {
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/lock-boot-worker.php"),
    )
    .workers(2)
    .env("HOLD_BOOT", "1")
    .http_pool("max_boot_failures = 2")
    .json_log()
    .spawn();
    assert_exit_code(srv.wait_exit(Duration::from_secs(10)), EXIT_FAILBOOT, &srv);
    assert_eq!(
        server_log::app_contexts(&srv.log_file(), "waiting at boot").len(),
        1
    );
    assert_eq!(
        server_log::app_contexts(&srv.log_file(), "boot lock held").len(),
        2
    );
}

/// One worker serves each request while the other retries past the boot failure limit.
#[test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
fn a_failing_worker_retries_next_to_a_serving_worker() {
    let observability = std::net::SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/lock-boot-worker.php"),
    )
    .workers(2)
    .http_pool("max_boot_failures = 2")
    .json_log()
    .toml(&format!(
        "[observability]\nlisten = \"{observability}\"\n[observability.probes]"
    ))
    .spawn();
    wait_serving(&srv, "http", 1);
    poll(Duration::from_secs(10), || {
        (server_log::app_contexts(&srv.log_file(), "boot lock held").len() >= 3).then_some(())
    })
    .unwrap_or_else(|| panic!("no retry past the limit\n{}", diagnostics(&srv)));
    let StatusBody { status: code, body } =
        http_get(observability, "/readyz", Duration::from_secs(10)).expect("GET /readyz");
    assert_eq!(
        (code, body.as_slice()),
        (200, b"ok\n".as_slice()),
        "\n{}",
        diagnostics(&srv)
    );

    for _ in 0..2 {
        let response = http_get(srv.addr, "/", BOOT).expect("GET /");
        assert_eq!(response.status, 200, "\n{}", diagnostics(&srv));
    }

    let held = submit(srv.addr, req("/hold")).expect("GET /hold");
    server_log::wait_app_record(&srv.log_file(), "held");
    let queued = submit(srv.addr, req("/")).expect("GET while the worker is busy");
    pool_line(&srv, "http", "pending=1 ");
    std::fs::write(srv.dir.join("http/release"), "").expect("release the held request");
    let deadline = Instant::now() + BOOT;
    let statuses = [held, queued].map(|mut reply| {
        tests::drain_resp_deadline(&mut reply, deadline)
            .expect("response after the release")
            .status()
    });
    assert_eq!(statuses, [200, 200], "\n{}", diagnostics(&srv));
}

/// An entrypoint that returns before `handle_request()` fails its boot without a client.
#[test]
fn worker_bootstrap_that_never_serves_failboots() {
    let Exited { status, log } = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/never-loop-worker.php"),
    )
    .http_pool("max_boot_failures = 1")
    .boot_failure();
    assert_eq!(status.code(), Some(EXIT_FAILBOOT), "{log}");
}

/// A client that leaves in the middle of the handler must not stop the worker. The abort ends only that unit, and the next request is served.
#[test]
fn worker_survives_client_leaving() {
    let srv = Spawn::http(Mode::Worker, fixture_path("lifecycle/hold-worker.php")).spawn();
    let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
    c.send(b"GET / HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    assert!(
        wait_log_contains(&srv, "held", Duration::from_secs(10)),
        "\n{}",
        diagnostics(&srv)
    );
    c.leave();

    let StatusBody { status: code, body } =
        http_get(srv.addr, "/?probe=1", Duration::from_secs(10)).expect("GET");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(body, b"ok", "\n{}", diagnostics(&srv));
}

/// An unfinalized exchange whose client left is finalized, so `activeCount()` does not count it. The next `receive()` discards it, and the discard is logged.
#[test]
fn cancelled_exchange_is_discarded_by_next_receive() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/cancelled-receive-worker.php"),
    )
    .toml("[log]\nlevel = \"debug\"")
    .no_rust_log()
    .spawn();
    wait_serving(&srv, "http", 1);
    let mut client = Conn::open(srv.addr, BOOT).expect("connect");
    client
        .send(b"GET /events HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send /events");
    let Head { status, fields } = client
        .read_head(BOOT)
        .unwrap_or_else(|e| panic!("/events head: {e}\n{}", diagnostics(&srv)));
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v == "chunked")
            && !fields.iter().any(|(k, _)| k == "content-length"),
        "the response must remain chunked: {fields:?}\n{}",
        diagnostics(&srv)
    );
    client
        .read_body_until(b"data: connected\n\n", BOOT)
        .unwrap_or_else(|e| panic!("/events body: {e}\n{}", diagnostics(&srv)));
    // The bytes can reach the client before writeBody() returns to PHP.
    assert!(
        wait_log_contains(&srv, "waiting for exchange cancellation", BOOT),
        "{}",
        diagnostics(&srv)
    );
    client.leave();

    let StatusBody { status, body } = http_get(srv.addr, "/next", BOOT)
        .unwrap_or_else(|e| panic!("GET /next: {e}\n{}", diagnostics(&srv)));
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        body,
        b"2:cancelled active=0",
        "the next request must preserve the worker and script cycle\n{}",
        diagnostics(&srv)
    );
    assert!(
        wait_log_contains(&srv, "discarded an unfinalized unit", BOOT),
        "the discard must be logged\n{}",
        diagnostics(&srv)
    );
}

/// A field sent more than once reaches PHP as one value: a comma list, and `"; "` for Cookie.
#[test]
fn repeated_request_fields_reach_php_combined() {
    let srv = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/repeated-headers-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);
    let StatusBody { status: code, body } = http_get_with_headers(
        srv.addr,
        "/",
        &[
            ("Cookie", "a=1"),
            ("Cookie", "b=2"),
            ("X-Forwarded-For", "203.0.113.7"),
            ("X-Forwarded-For", "10.0.0.1"),
        ],
        Duration::from_secs(10),
    )
    .expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        "1,2\na=1; b=2\n203.0.113.7, 10.0.0.1\n",
        "\n{}",
        diagnostics(&srv)
    );
}

/// A wire name with `_` or `.` must never reach the CGI variable of a `-` name. The SAPI function cgi_header_name rewrites `.` to `_`.
#[test]
fn alias_names_never_reach_a_cgi_variable() {
    let srv = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/repeated-headers-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);
    let StatusBody { status: code, body } = http_get_with_headers(
        srv.addr,
        "/",
        &[
            ("X_Forwarded_For", "1.2.3.4"),
            ("X.Forwarded.For", "5.6.7.8"),
        ],
        Duration::from_secs(10),
    )
    .expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        "-,-\n-\n-\n",
        "no alias may reach HTTP_X_FORWARDED_FOR\n{}",
        diagnostics(&srv)
    );
}

/// The cap of the accumulate loop. With no declared length, the 413 comes when the streamed body crosses max_body_size.
#[test]
fn chunked_body_over_the_cap_answers_413() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .http_extra("max_body_size_mb = 1")
    .spawn();
    wait_serving(&srv, "http", 1);

    let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
    c.send(b"POST / HTTP/1.1\r\nHost: e2e\r\nTransfer-Encoding: chunked\r\n\r\n")
        .expect("head");
    let chunk = vec![b'a'; 64 * 1024];
    let size_line = format!("{:x}\r\n", chunk.len());
    // 17 x 64 KiB crosses the 1 MiB cap on the last chunk; the 413 arrives without a terminal chunk ever being sent.
    for _ in 0..17 {
        if c.send(size_line.as_bytes()).is_err()
            || c.send(&chunk).is_err()
            || c.send(b"\r\n").is_err()
        {
            break;
        }
    }
    let status = c
        .read_head(Duration::from_secs(10))
        .expect("413 head")
        .status;
    assert_eq!(status, 413, "\n{}", diagnostics(&srv));
}

/// An under-cap chunked body dispatches normally. This shows that the cap check is not too strict.
#[test]
fn chunked_body_under_the_cap_is_served() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .http_extra("max_body_size_mb = 1")
    .spawn();
    wait_serving(&srv, "http", 1);

    let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
    c.send(
        b"POST / HTTP/1.1\r\nHost: e2e\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    )
    .expect("head");
    c.send(b"5\r\nhello\r\n0\r\n\r\n").expect("body");
    let status = c.read_head(Duration::from_secs(10)).expect("head").status;
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    c.read_body_until(b"ok", Duration::from_secs(10))
        .expect("body");
}

/// A chunked body that does not decode answers 400 before dispatch. The fixture answers 200 to each request it gets, so a 400 shows that PHP did not get the request.
/// RFC 9112 §7.1: https://www.rfc-editor.org/rfc/rfc9112#section-7.1
#[test]
fn malformed_chunked_body_answers_400_before_dispatch() {
    struct Case {
        name: &'static str,
        body: &'static [u8],
    }
    let cases = [
        Case {
            name: "a chunk size that is not hex, after a valid chunk",
            body: b"5\r\nhello\r\nzz\r\n",
        },
        Case {
            name: "chunk data longer than its size",
            body: b"5\r\nhelloXX\r\n0\r\n\r\n",
        },
    ];
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);

    for case in cases {
        let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
        c.send(b"POST / HTTP/1.1\r\nHost: e2e\r\nTransfer-Encoding: chunked\r\n\r\n")
            .expect("head");
        c.send(case.body).expect("body");
        let status = c
            .read_head(Duration::from_secs(10))
            .unwrap_or_else(|e| panic!("{}: {e}\n{}", case.name, diagnostics(&srv)))
            .status;
        assert_eq!(status, 400, "{}\n{}", case.name, diagnostics(&srv));
    }
}

/// The interim 100 goes out when the body is first polled, which happens only after admission.
#[test]
fn expect_100_continue_gets_the_interim_then_the_response() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);

    let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
    c.send(b"POST / HTTP/1.1\r\nHost: e2e\r\nExpect: 100-continue\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
        .expect("head");
    let status = c
        .read_head(Duration::from_secs(10))
        .expect("interim head")
        .status;
    assert_eq!(status, 100, "\n{}", diagnostics(&srv));
    c.send(b"hello").expect("body");
    let status = c
        .read_head(Duration::from_secs(10))
        .expect("final head")
        .status;
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
}

/// A refused request must never see a 100, because admission rejects it before the first poll of the body.
#[test]
fn expect_100_continue_is_skipped_when_the_request_is_refused() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .http_extra("max_body_size_mb = 1")
    .spawn();
    wait_serving(&srv, "http", 1);

    let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
    c.send(
        b"POST / HTTP/1.1\r\nHost: e2e\r\nExpect: 100-continue\r\nContent-Length: 2097152\r\n\r\n",
    )
    .expect("head");
    let status = c
        .read_head(Duration::from_secs(10))
        .expect("direct refusal")
        .status;
    assert_eq!(
        status,
        413,
        "no interim before the refusal\n{}",
        diagnostics(&srv)
    );
}

/// A final 1xx from PHP cannot go on the wire. hyper would rewrite it to a 500 and fail the connection, so the plugin serves 502.
#[test]
fn final_interim_status_becomes_502() {
    let srv = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/status-1xx-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);

    let raw = http_raw_bytes(
        srv.addr,
        b"GET / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        Duration::from_secs(10),
    )
    .expect("response");
    let head = String::from_utf8_lossy(&raw);
    assert!(
        head.starts_with("HTTP/1.1 502"),
        "got {:?}\n{}",
        head.lines().next().unwrap_or(""),
        diagnostics(&srv)
    );
}

/// More than one Host line is a 400 in every HTTP version. hyper hands the repeated field through, so the rejection is the plugin's own. https://www.rfc-editor.org/rfc/rfc9112#section-3.2
#[test]
fn a_second_host_field_line_answers_400() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);

    let code = http_get_with_headers(
        srv.addr,
        "/",
        &[("Host", "evil.example")],
        Duration::from_secs(10),
    )
    .expect("GET / with two Host field lines")
    .status;
    assert_eq!(code, 400, "\n{}", diagnostics(&srv));
}

/// `header("Status: 404")` must become the response code, not a literal field on a 200. The php-src function sapi_header_op passes it through verbatim, and the origin server converts it (RFC 3875 §6.2.1).
#[test]
fn status_field_sets_the_code_and_never_reaches_the_client() {
    let srv = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/status-header-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);
    let raw = http_get_raw(srv.addr, "/", &[], Duration::from_secs(10)).expect("GET /");
    let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();

    assert!(
        text.starts_with("http/1.1 404"),
        "Status: must set the response code (got {:?})\n{}",
        text.lines().next().unwrap_or(""),
        diagnostics(&srv)
    );
    assert!(
        !text.contains("\r\nstatus:"),
        "Status: must not reach the client\n{}",
        diagnostics(&srv)
    );
    assert!(
        text.contains("x-keep: kept"),
        "other fields must survive\n{}",
        diagnostics(&srv)
    );
}

/// Request fidelity over a real socket: repeated field lines reach PHP as a list, receivedAt is a plausible ingress stamp, and a Host-less HTTP/1.1 request is answered 400 before dispatch.
/// RFC 9112 §3.2: https://www.rfc-editor.org/rfc/rfc9112#section-3.2
#[test]
fn dispatcher_request_fidelity_over_the_wire() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .spawn();

    let before = std::time::UNIX_EPOCH.elapsed().unwrap().as_secs_f64();
    let StatusBody { status: code, body } = http_get_with_headers(
        srv.addr,
        "/?probe=headers",
        &[
            ("X-Probe", "one"),
            ("X-Probe", "two"),
            ("x_forwarded_for", "1.2.3.4"),
        ],
        Duration::from_secs(10),
    )
    .expect("GET headers probe");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        "x-probe=one|two\nx_forwarded_for=1.2.3.4"
    );

    let StatusBody { status: code, body } =
        http_get(srv.addr, "/?probe=received", Duration::from_secs(10)).expect("GET received");
    assert_eq!(code, 200);
    let after = std::time::UNIX_EPOCH.elapsed().unwrap().as_secs_f64();
    let received: f64 = String::from_utf8_lossy(&body)
        .trim_start_matches("received=")
        .parse()
        .expect("receivedAt is a float");
    assert!(
        received >= before && received <= after,
        "receivedAt {received} outside [{before}, {after}]"
    );

    let code = http_raw(
        srv.addr,
        b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n",
        Duration::from_secs(10),
    )
    .expect("Host-less request")
    .status;
    assert_eq!(code, 400, "missing Host on HTTP/1.1 must answer 400");
}

/// A PHP-written head crosses the wire: the status line, one field line per list value, and the plugin's own framing.
/// RFC 9110 §8.6: https://www.rfc-editor.org/rfc/rfc9110#section-8.6
#[test]
fn dispatcher_write_head_reaches_the_wire() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);

    let raw = http_get_raw(srv.addr, "/?probe=head", &[], Duration::from_secs(10))
        .expect("GET head probe");
    let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    assert!(
        text.starts_with("http/1.1 201"),
        "got {:?}\n{}",
        text.lines().next().unwrap_or(""),
        diagnostics(&srv)
    );
    assert_eq!(
        text.matches("\r\nx-a: ").count(),
        2,
        "one field line per list value\n{}",
        diagnostics(&srv)
    );
    assert!(
        text.contains("\r\ncontent-length: 999\r\n"),
        "the declared content-length must be honoured\n{}",
        diagnostics(&srv)
    );
    assert!(text.ends_with("body"), "{text:?}\n{}", diagnostics(&srv));
}

/// A HEAD response keeps the content-length that the app declared for the GET response, and carries no body.
/// RFC 9110 §9.3.2: https://www.rfc-editor.org/rfc/rfc9110#section-9.3.2
#[test]
fn head_keeps_the_declared_content_length() {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        mode: Mode,
        status: &'static str,
        content_length: &'static str,
    }
    let cases = [
        Case {
            name: "dispatcher writeHead",
            fixture: "lifecycle/fidelity-worker.php",
            mode: Mode::Dispatcher,
            status: "http/1.1 201",
            content_length: "999",
        },
        Case {
            name: "classic header()",
            fixture: "lifecycle/head-length-classic.php",
            mode: Mode::Classic,
            status: "http/1.1 200",
            content_length: "11",
        },
    ];
    for case in cases {
        let srv = Spawn::http(case.mode, fixture_path(case.fixture)).spawn();
        let raw = http_raw_bytes(
            srv.addr,
            b"HEAD /?probe=head HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
            Duration::from_secs(10),
        )
        .unwrap_or_else(|e| panic!("{}: HEAD: {e}\n{}", case.name, diagnostics(&srv)));
        let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(
            text.starts_with(case.status),
            "{}: got {:?}\n{}",
            case.name,
            text.lines().next().unwrap_or(""),
            diagnostics(&srv)
        );
        assert!(
            text.contains(&format!("\r\ncontent-length: {}\r\n", case.content_length)),
            "{}: the declared content-length must stay on HEAD: {text:?}\n{}",
            case.name,
            diagnostics(&srv)
        );
        assert!(
            text.ends_with("\r\n\r\n"),
            "{}: no body bytes on a HEAD response: {text:?}\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
}

/// The method token is case-sensitive, so `head` is an extension method, not HEAD. The body that the app writes reaches the client.
/// RFC 9110 §9.1: https://www.rfc-editor.org/rfc/rfc9110#section-9.1
#[test]
fn lowercase_head_is_not_head() {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        mode: Mode,
        body: &'static str,
    }
    let cases = [
        Case {
            name: "dispatcher writeBody",
            fixture: "lifecycle/fidelity-worker.php",
            mode: Mode::Dispatcher,
            body: "ok",
        },
        Case {
            name: "classic echo",
            fixture: "lifecycle/head-length-classic.php",
            mode: Mode::Classic,
            body: "hello world",
        },
    ];
    for case in cases {
        let srv = Spawn::http(case.mode, fixture_path(case.fixture)).spawn();
        let raw = http_raw_bytes(
            srv.addr,
            b"head / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
            Duration::from_secs(10),
        )
        .unwrap_or_else(|e| panic!("{}: head: {e}\n{}", case.name, diagnostics(&srv)));
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.starts_with("HTTP/1.1 200") && text.ends_with(&format!("\r\n\r\n{}", case.body)),
            "{}: want the {:?} body: {text:?}\n{}",
            case.name,
            case.body,
            diagnostics(&srv)
        );
    }
}

/// Host-side multipart over the wire: a non-UTF-8 boundary round-trips, the spool file goes at finalization, malformed framing answers 400 and an over-limit file part 413.
#[test]
fn dispatcher_multipart_over_the_wire() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .http_extra("[http.uploads]\nmax_file_size_mb = 1")
    .spawn();

    let ct = b"multipart/form-data; boundary=RAP\xff\xfeIRA".to_vec();
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--RAP\xff\xfeIRA\r\ncontent-disposition: form-data; name=\"note\"\r\n\r\nhello\r\n",
    );
    body.extend_from_slice(b"--RAP\xff\xfeIRA\r\ncontent-disposition: form-data; name=\"f\"; filename=\"a.bin\"\r\n\r\nPAYLOAD\r\n");
    body.extend_from_slice(b"--RAP\xff\xfeIRA--");
    let StatusBody {
        status: code,
        body: resp,
    } = http_post(
        srv.addr,
        "/?probe=multipart",
        &ct,
        &body,
        Duration::from_secs(10),
    )
    .expect("multipart POST");
    let text = String::from_utf8_lossy(&resp).into_owned();
    assert_eq!(code, 200, "{text}\n{}", diagnostics(&srv));
    assert!(text.contains("field=note=hello"), "{text}");
    assert!(text.contains("file-content=PAYLOAD"), "{text}");
    let tmp = text
        .lines()
        .find_map(|l| l.strip_prefix("tmp="))
        .expect("tmp line");
    // The client can get the last byte of the response before seal() unlinks the spool file.
    assert!(
        poll(Duration::from_secs(5), || {
            (!std::path::Path::new(tmp).exists()).then_some(())
        })
        .is_some(),
        "spool file must be gone after the response"
    );

    let code = http_post(
        srv.addr,
        "/?probe=multipart",
        b"multipart/form-data; boundary=B",
        b"no boundary line at all",
        Duration::from_secs(10),
    )
    .expect("malformed POST")
    .status;
    assert_eq!(code, 400, "malformed multipart must answer 400");

    let mut big = Vec::new();
    big.extend_from_slice(
        b"--B\r\ncontent-disposition: form-data; name=\"f\"; filename=\"a\"\r\n\r\n",
    );
    big.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
    big.extend_from_slice(b"\r\n--B--");
    let code = http_post(
        srv.addr,
        "/?probe=multipart",
        b"multipart/form-data; boundary=B",
        &big,
        Duration::from_secs(10),
    )
    .expect("over-limit POST")
    .status;
    assert_eq!(code, 413, "over-limit file part must answer 413");
}

/// A relative TMPDIR names the default spool dir of file parts. The host resolves it against its launch dir at boot, so a spool path stays valid after PHP enters the entrypoint directory.
#[test]
fn a_relative_tmpdir_gives_an_absolute_spool_path() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/fidelity-worker.php"),
    )
    .env("TMPDIR", "spool")
    .spawn();
    let spool = std::fs::canonicalize(&srv.dir)
        .expect("canonicalize the scratch dir")
        .join("spool");

    let body = b"--B\r\ncontent-disposition: form-data; name=\"note\"\r\n\r\nhello\r\n\
--B\r\ncontent-disposition: form-data; name=\"f\"; filename=\"a.bin\"\r\n\r\nPAYLOAD\r\n--B--";
    let StatusBody { status, body } = http_post(
        srv.addr,
        "/?probe=multipart",
        b"multipart/form-data; boundary=B",
        body,
        Duration::from_secs(10),
    )
    .expect("multipart POST");
    let text = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, 200, "{text}\n{}", diagnostics(&srv));
    let tmp = text
        .lines()
        .find_map(|l| l.strip_prefix("tmp="))
        .unwrap_or_else(|| panic!("no tmp line: {text}"));
    assert!(
        std::path::Path::new(tmp).starts_with(&spool),
        "the spool path {tmp} is not under {}\n{}",
        spool.display(),
        diagnostics(&srv)
    );
}

/// The request watchdog does not count the boot of the next app instance. The last request and the boot take 2 s each, under the 3 s limit, and the worker survives.
#[test]
fn the_request_watchdog_skips_the_boot_after_an_app_restart() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/slow-reboot-worker.php"),
    )
    .http_pool("max_requests = 1\nrequest_terminate_timeout_secs = 3")
    .json_log()
    .spawn();
    wait_serving(&srv, "http", 1);
    // max_requests = 1 is a quota of 2, so the slow second request ends the app instance.
    for path in ["/", "/?slow"] {
        let code = http_get(srv.addr, path, Duration::from_secs(10))
            .expect("GET")
            .status;
        assert_eq!(code, 200, "{path}\n{}", diagnostics(&srv));
    }
    slot_line(&srv, "state=idle handled=2 errors=0 recycles=1 ");
    assert_eq!(server_log::app_contexts(&srv.log_file(), "booted").len(), 2);
    assert_eq!(
        http_get(srv.addr, "/", BOOT)
            .expect("GET after boot")
            .status,
        200
    );
}

/// A server reload during a failed boot starts the repaired app.
#[test]
fn a_reload_during_a_failing_boot_boots_the_fixed_app() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/breakable-boot-worker.php"),
    )
    .json_log()
    .spawn();
    wait_serving(&srv, "http", 1);
    let log = srv.log_file();
    tests::server_log::wait_app_record(&log, "booted");
    let flag = srv.dir.join("http/broken.flag");
    std::fs::write(&flag, "").expect("break the boot");
    signal(srv.pid(), libc::SIGUSR2);
    assert!(
        wait_log_contains(&srv, "broken deploy", Duration::from_secs(10)),
        "no failed boot\n{}",
        diagnostics(&srv)
    );
    std::fs::remove_file(&flag).expect("fix the boot");
    signal(srv.pid(), libc::SIGUSR2);
    let booted = poll(Duration::from_secs(10), || {
        (tests::server_log::app_contexts(&log, "booted").len() >= 2).then_some(())
    });
    assert!(
        booted.is_some(),
        "the fixed app booted\n{}",
        diagnostics(&srv)
    );
    let text = std::fs::read_to_string(&log).expect("read the log");
    assert!(
        !text.contains("reload skipped"),
        "a false reload warning\n{}",
        diagnostics(&srv)
    );
}
