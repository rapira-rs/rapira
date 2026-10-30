use crate::harness::*;
use rapira_sapi::plugin::Mode;
use std::time::Duration;
use tests::wire::submit;
use tests::{StatusBody, drain_resp, poll, req, server_log};

#[test]
fn retained_spl_tempfile_survives_next_request() {
    let srv = spawn_with_config(
        "lifecycle/retained-temp-worker.php",
        1,
        "mode = \"worker\"\n",
    );
    let timeout = Duration::from_secs(10);
    let pid = wait_workers(&srv, timeout, "1 worker", |p| p.len() == 1)[0];

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
            format!("{pid}:{}:stored", index * 2 + 1).into_bytes(),
            "\n{}",
            diagnostics(&srv)
        );

        let StatusBody { status: code, body } = http_get(srv.addr, "/export", timeout)
            .unwrap_or_else(|e| panic!("GET /export: {e}\n{}", diagnostics(&srv)));
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
        assert_eq!(
            body,
            format!("{pid}:{}:{contents}", index * 2 + 2).into_bytes(),
            "\n{}",
            diagnostics(&srv)
        );
    }
}

#[test]
fn killed_worker_respawns() {
    let srv = spawn_with_config("shared/echo-worker.php", 2, "");
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    signal(pids0[0], libc::SIGKILL);
    wait_workers(
        &srv,
        Duration::from_secs(20),
        "respawn to 2 with a fresh pid",
        |p| p.len() == 2 && p.iter().any(|x| !pids0.contains(x)),
    );
    let crash = format!(
        "http pool: worker {} killed by signal {}",
        pids0[0],
        libc::SIGKILL
    );
    assert!(
        wait_log_contains(&srv, &crash, Duration::from_secs(5)),
        "no {crash:?} record\n{}",
        diagnostics(&srv)
    );
    for _ in 0..5 {
        let code = http_get(srv.addr, "/", Duration::from_secs(10))
            .expect("GET /")
            .status;
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
}

/// Workers ignore SIGUSR1, and SIGUSR2 only asks them to restart their app, so `pkill -USR2 rapira` does not stop them. Classic mode starts PHP again for each request, and PHP sets its signal handlers again at each start.
#[test]
fn workers_ignore_usr_signals() {
    struct Case {
        name: &'static str,
        signal: i32,
    }
    let cases = [
        Case {
            name: "SIGUSR1",
            signal: libc::SIGUSR1,
        },
        Case {
            name: "SIGUSR2",
            signal: libc::SIGUSR2,
        },
    ];
    let srv = spawn_with_config("classic/cwd.php", 1, "mode = \"classic\"\n");
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    for _ in 0..2 {
        let code = http_get(srv.addr, "/", Duration::from_secs(10))
            .expect("GET /")
            .status;
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
    for case in cases {
        signal(pid, case.signal);
        // A worker that the signal stops exits in a few milliseconds.
        std::thread::sleep(Duration::from_millis(200));
        let code = http_get(srv.addr, "/", Duration::from_secs(10))
            .unwrap_or_else(|e| panic!("{}: GET /: {e}\n{}", case.name, diagnostics(&srv)))
            .status;
        assert_eq!(code, 200, "{}\n{}", case.name, diagnostics(&srv));
        assert_eq!(
            worker_pids(srv.pid()),
            [pid],
            "{}: the worker must survive\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
}

// After the master exits its workers reparent away and `worker_pids` cannot see them, so poll the captured pids directly.
fn wait_pids_gone(pids: &[u32], timeout: Duration, srv: &Server) {
    poll(timeout, || {
        // SAFETY: kill(pid, 0) only probes existence; ESRCH means gone.
        pids.iter()
            .all(|&p| unsafe { libc::kill(p as libc::pid_t, 0) } == -1)
            .then_some(())
    })
    .unwrap_or_else(|| {
        panic!(
            "workers survived the master: {pids:?}\n{}",
            diagnostics(srv)
        )
    });
}

#[test]
fn sigquit_master_graceful() {
    let mut srv = spawn_with_config("shared/echo-worker.php", 2, "");
    let pids = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET /")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    signal(srv.pid(), libc::SIGQUIT);
    let status = srv.wait_exit(STOP_BUDGET);
    assert_exit_code(status, MASTER_EXIT_OK, &srv);
    wait_pids_gone(&pids, Duration::from_secs(10), &srv);
}

#[test]
fn sigterm_master_stops() {
    let mut srv = spawn_with_config("shared/echo-worker.php", 2, "");
    let pids = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    signal(srv.pid(), libc::SIGTERM);
    let status = srv.wait_exit(STOP_BUDGET);
    assert_exit_code(status, MASTER_EXIT_OK, &srv);
    wait_pids_gone(&pids, Duration::from_secs(10), &srv);
}

/// The pidfile holds the pid of the master until the master exits after a stop. A second SIGTERM during the stop forces the exit with code 130 and stops the workers. A held request keeps the stop open.
#[test]
fn the_pidfile_names_the_master_until_its_exit() {
    struct Case {
        name: &'static str,
        second_sigterm: bool,
        want_exit: i32,
    }
    let cases = [
        Case {
            name: "stop",
            second_sigterm: false,
            want_exit: MASTER_EXIT_OK,
        },
        Case {
            name: "forced stop",
            second_sigterm: true,
            want_exit: 130,
        },
    ];
    for case in cases {
        let mut srv = Spawn::http(Mode::Worker, tests::fixture("worker/held-worker.php"))
            .toml("[supervisor]\npidfile = \"rapira.pid\"")
            .json_log()
            .spawn();
        let pidfile = srv.dir.join("rapira.pid");
        let release = srv.dir.join("release");
        let pids = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
        let mut held = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
        held.send(
            format!(
                "GET /?release={} HTTP/1.1\r\nHost: e2e\r\n\r\n",
                release.display()
            )
            .as_bytes(),
        )
        .expect("send the held request");
        tests::server_log::wait_app_record(&srv.log_file(), "held");

        signal(srv.pid(), libc::SIGTERM);
        // Two pending SIGTERMs merge into one, so the second goes after the master began the stop.
        assert!(
            wait_log_contains(&srv, "draining worker", Duration::from_secs(10)),
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
        // Without the release the held request ends only with its worker.
        if case.second_sigterm {
            signal(srv.pid(), libc::SIGTERM);
        } else {
            std::fs::write(&release, "").expect("write the release file");
        }
        let status = srv.wait_exit(STOP_BUDGET);
        assert_exit_code(status, case.want_exit, &srv);
        assert!(
            !pidfile.exists(),
            "{}: the pidfile stays after the exit",
            case.name
        );
        wait_pids_gone(&pids, Duration::from_secs(10), &srv);
    }
}

/// Worker exit code after a clean drain (`rapira_master::WORKER_EXIT_DRAINED`).
#[cfg(target_os = "linux")]
const WORKER_EXIT_DRAINED: i32 = 0;

/// The test process is a child subreaper while the master dies, so the orphaned workers become its children and their exit codes are visible. https://man7.org/linux/man-pages/man2/PR_SET_CHILD_SUBREAPER.2const.html
#[cfg(target_os = "linux")]
#[test]
fn killed_master_leaves_workers_to_drain() {
    let mut srv = spawn_with_config("shared/echo-worker.php", 2, "");
    let pids = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET /")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));

    kill_master_as_subreaper(&mut srv);

    for pid in pids {
        assert_eq!(
            wait_child_exit(pid, Duration::from_secs(20), &srv),
            Some(WORKER_EXIT_DRAINED),
            "worker {pid} did not drain after the master died\n{}",
            diagnostics(&srv)
        );
    }
}

/// max_requests = 2 is a quota of 3. A served unit uses the quota, but a queued unit whose client left does not: after A and two such units, the count probe still reaches the app instance that served A.
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
        // No signal shows that B and C are queued or that the server saw their close. A holds the worker until the release file exists, so each wait can be long.
        std::thread::sleep(Duration::from_secs(1));
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

/// max_requests = 2 is a quota of 3, so the storm restarts the app many times. Each restart boots the app again in the same process, and no request fails.
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
        let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
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
        assert_eq!(
            worker_pids(srv.pid()),
            vec![pid],
            "{}: the worker process stays\n{}",
            case.name,
            diagnostics(&srv)
        );
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

#[test]
fn request_timeout_kills_and_replaces_worker() {
    let srv = spawn_with_config(
        "lifecycle/hang-worker.php",
        1,
        "request_terminate_timeout_secs = 2\n",
    );
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET /")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    let hung = http_get(srv.addr, "/?hang=1", Duration::from_secs(15));
    assert!(
        hung.is_err(),
        "hung request must die with the worker, got {hung:?}\n{}",
        diagnostics(&srv)
    );
    wait_workers(
        &srv,
        Duration::from_secs(20),
        "worker replaced after timeout kill",
        |p| p.len() == 1 && p[0] != pids0[0],
    );
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET / after kill")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
}

/// A worker whose RSS goes above max_memory_mb drains, and the master forks a new one. The drain starts while the request that grew the memory still runs, and that request completes. The new worker stays under the limit.
#[test]
fn a_worker_above_max_memory_is_replaced() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/memory-growth-worker.php"),
    )
    .http_pool("max_memory_mb = 128")
    .spawn();
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    let code = http_get(srv.addr, "/?grow=160", Duration::from_secs(10))
        .expect("GET /?grow=160")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    let next = wait_workers(
        &srv,
        Duration::from_secs(10),
        "worker replaced above the memory limit",
        |p| p.len() == 1 && p[0] != pid,
    )[0];
    assert!(
        wait_log_contains(&srv, "http.pool.max_memory_mb", Duration::from_secs(5)),
        "the master logs why it drained the worker\n{}",
        diagnostics(&srv)
    );
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET / after the replacement")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    // Three master ticks: a new worker under the limit is not drained.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        worker_pids(srv.pid()),
        vec![next],
        "the new worker stays\n{}",
        diagnostics(&srv)
    );
}

/// A top-level `[pool]` table is a parse error: the pool belongs under `[http.pool]`.
#[test]
fn a_top_level_pool_table_refuses_to_boot() {
    let Exited { status, log } =
        spawn_boot_failure("shared/echo-worker.php", "[pool]\nprocesses = 1\n");
    assert_eq!(status.code(), Some(1), "\n{log}");
    assert!(log.contains("parsing config file"), "\n{log}");
    assert!(log.contains("unknown field `pool`"), "\n{log}");
}

/// The entrypoint is fixed for the pool's lifetime, so the master proves it is readable at boot.
#[test]
fn a_missing_entrypoint_refuses_to_boot() {
    let Exited { status, log } = spawn_boot_failure_with_entrypoint("no-such-entrypoint.php");
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
    let Exited { status, log } = spawn_boot_failure_with_entrypoint(&script.display().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "\n{log}");
    assert!(log.contains("http.pool.entrypoint"), "\n{log}");
    assert!(log.contains("is not readable"), "\n{log}");
}

/// A gen-0 pool whose boot always fails stops the master with exit code 70. No request is necessary: each worker runs the boot again on its own, and the fifth failed boot flags it unhealthy. Two workers: a worker that fails its boot does not count as serving.
#[test]
fn master_failboot_exits_70() {
    let mut srv = spawn_with_config("lifecycle/fatal-worker.php", 2, "");
    let status = srv.wait_exit(Duration::from_secs(60));
    assert_exit_code(status, MASTER_EXIT_FAILBOOT, &srv);
    let log = std::fs::read_to_string(srv.log_file()).expect("read server.log");
    assert!(
        log.contains("worker keeps failing to boot; flagged unhealthy"),
        "5 failed boots must flag the worker unhealthy\n{log}"
    );
}

/// A gen-0 pool with a serving worker keeps the master up when another worker never boots. The fixture fails its boot while the other worker holds boot.lock. No request arrives.
#[test]
fn a_worker_that_never_boots_next_to_a_serving_worker_keeps_the_master_up() {
    let observability = std::net::SocketAddr::from(([127, 0, 0, 1], free_port()));
    let mut srv = spawn_with_config(
        "lifecycle/lock-boot-worker.php",
        2,
        &format!(
            "mode = \"dispatcher\"\n[observability]\nlisten = \"{observability}\"\n[observability.probes]\n"
        ),
    );
    assert!(
        wait_log_contains(
            &srv,
            "worker keeps failing to boot; flagged unhealthy",
            Duration::from_secs(40)
        ),
        "\n{}",
        diagnostics(&srv)
    );
    let status = srv.wait_exit(Duration::from_secs(5));
    assert!(
        status.is_none(),
        "the master exited: {status:?}\n{}",
        diagnostics(&srv)
    );
    let StatusBody { status: code, body } =
        http_get(observability, "/readyz", Duration::from_secs(10)).expect("GET /readyz");
    assert_eq!(
        (code, body.as_slice()),
        (200, b"ok\n".as_slice()),
        "\n{}",
        diagnostics(&srv)
    );
}

/// A worker-mode bootstrap that never calls handle_request() must failboot the master, not hang or shed 503s forever.
#[test]
fn worker_bootstrap_that_never_serves_failboots() {
    let mut srv = spawn_with_config("lifecycle/never-loop-worker.php", 1, "mode = \"worker\"\n");
    let addr = srv.addr;
    let status = poll(Duration::from_secs(60), || {
        if let Some(st) = srv.try_status() {
            return Some(st);
        }
        let _ = http_get(addr, "/", Duration::from_secs(2));
        None
    })
    .unwrap_or_else(|| panic!("master never exited\n{}", diagnostics(&srv)));
    assert_exit_code(Some(status), MASTER_EXIT_FAILBOOT, &srv);
}

/// A client that walks away mid-handler must not take the worker down: the abort ends only that unit, and the next request is served.
#[test]
fn worker_survives_client_leaving() {
    let srv = spawn_with_config("lifecycle/hold-worker.php", 1, "mode = \"worker\"\n");
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

/// An unfinalized exchange whose client left is finalized, so `activeCount()` no longer counts it. The next `receive()` discards it, and the discard is logged.
#[test]
fn cancelled_exchange_is_discarded_by_next_receive() {
    let srv = spawn_without_rust_log(
        "lifecycle/cancelled-receive-worker.php",
        1,
        "mode = \"dispatcher\"\n[log]\nlevel = \"debug\"\n",
    );
    let pid = wait_workers(&srv, BOOT, "1 dispatcher worker", |p| p.len() == 1)[0];
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
    client.leave();

    let StatusBody { status, body } = http_get(srv.addr, "/next", BOOT)
        .unwrap_or_else(|e| panic!("GET /next: {e}\n{}", diagnostics(&srv)));
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        body,
        format!("{pid}:2:cancelled active=0").into_bytes(),
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
    let srv = spawn_with_config(
        "lifecycle/repeated-headers-worker.php",
        1,
        "mode = \"worker\"\n",
    );
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
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

/// A wire name carrying `_` or `.` must never reach the CGI variable a `-` name owns; PHP itself rewrites `.` to `_` when registering it.
#[test]
fn alias_names_never_reach_a_cgi_variable() {
    let srv = spawn_with_config(
        "lifecycle/repeated-headers-worker.php",
        1,
        "mode = \"worker\"\n",
    );
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
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

/// `reject` puts a real 400 on the wire: the field-name check runs before dispatch, so PHP never sees the request.
#[test]
fn reject_policy_answers_400_for_an_alias_name() {
    let srv = spawn_with_http_extra(
        "shared/echo-worker.php",
        1,
        "unsafe_field_names = \"reject\"\n",
    );
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

    let code = http_get_with_headers(
        srv.addr,
        "/",
        &[("X_Forwarded_For", "1.2.3.4")],
        Duration::from_secs(10),
    )
    .expect("GET / with an alias name")
    .status;
    assert_eq!(code, 400, "\n{}", diagnostics(&srv));

    let code = http_get_with_headers(
        srv.addr,
        "/",
        &[("X-Forwarded-For", "203.0.113.7")],
        Duration::from_secs(10),
    )
    .expect("GET / with a safe name")
    .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
}

/// More than one `Host` line is a 400; the h1 parser hands the repeated field through intact, so the rejection is this plugin's own.
/// RFC 9112 §3.2: https://www.rfc-editor.org/rfc/rfc9112#section-3.2
#[test]
fn a_second_host_field_line_answers_400() {
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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

/// CONNECT answers 501 before dispatch: a PHP-authored 2xx would turn the connection into a tunnel, and a 405 would owe an Allow list (RFC 9110 §15.5.6).
/// https://www.rfc-editor.org/rfc/rfc9110#section-15.5.6
#[test]
fn connect_answers_501() {
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

    let raw = http_raw_bytes(
        srv.addr,
        b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
        Duration::from_secs(10),
    )
    .expect("CONNECT request");
    let head = String::from_utf8_lossy(&raw);
    assert!(
        head.starts_with("HTTP/1.1 501"),
        "got {:?}\n{}",
        head.lines().next().unwrap_or(""),
        diagnostics(&srv)
    );
}

/// The accumulate-loop cap: with no declared length the 413 fires when the streamed body crosses max_body_size.
#[test]
fn chunked_body_over_the_cap_answers_413() {
    let srv = spawn_with_http_extra("lifecycle/fidelity-worker.php", 1, "max_body_size_mb = 1\n");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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

/// An under-cap chunked body dispatches normally, pinning that the cap check is not over-eager.
#[test]
fn chunked_body_under_the_cap_is_served() {
    let srv = spawn_with_http_extra("lifecycle/fidelity-worker.php", 1, "max_body_size_mb = 1\n");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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

/// A refused request must never see a 100: admission rejects before the body is ever polled.
#[test]
fn expect_100_continue_is_skipped_when_the_request_is_refused() {
    let srv = spawn_with_http_extra("lifecycle/fidelity-worker.php", 1, "max_body_size_mb = 1\n");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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

/// A final 1xx from PHP cannot go on the wire: hyper would rewrite it to a 500 and error the connection, so the plugin serves 502.
#[test]
fn final_interim_status_becomes_502() {
    let srv = spawn_with_config("lifecycle/status-1xx-worker.php", 1, "mode = \"worker\"\n");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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

/// `header("Status: 404")` must become the response code, not a literal field on a 200: php-src's sapi_header_op passes it through verbatim and the origin server converts it (RFC 3875 §6.2.1).
#[test]
fn status_field_sets_the_code_and_never_reaches_the_client() {
    let srv = spawn_with_config(
        "lifecycle/status-header-worker.php",
        1,
        "mode = \"worker\"\n",
    );
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
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
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");

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
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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
        config: &'static str,
        status: &'static str,
        content_length: &'static str,
    }
    let cases = [
        Case {
            name: "dispatcher writeHead",
            fixture: "lifecycle/fidelity-worker.php",
            config: "",
            status: "http/1.1 201",
            content_length: "999",
        },
        Case {
            name: "classic header()",
            fixture: "lifecycle/head-length-classic.php",
            config: "mode = \"classic\"\n",
            status: "http/1.1 200",
            content_length: "11",
        },
    ];
    for case in cases {
        let srv = spawn_with_config(case.fixture, 1, case.config);
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

/// The method token is case-sensitive, so `head` is an extension method, not HEAD: the app's body reaches the client.
/// RFC 9110 §9.1: https://www.rfc-editor.org/rfc/rfc9110#section-9.1
#[test]
fn lowercase_head_is_not_head() {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        config: &'static str,
        body: &'static str,
    }
    let cases = [
        Case {
            name: "dispatcher writeBody",
            fixture: "lifecycle/fidelity-worker.php",
            config: "",
            body: "ok",
        },
        Case {
            name: "classic echo",
            fixture: "lifecycle/head-length-classic.php",
            config: "mode = \"classic\"\n",
            body: "hello world",
        },
    ];
    for case in cases {
        let srv = spawn_with_config(case.fixture, 1, case.config);
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

/// RFC 9112 §3.2.2: for an absolute-form target the origin server must ignore Host and use the target's host information.
/// https://www.rfc-editor.org/rfc/rfc9112#section-3.2.2
#[test]
fn absolute_form_target_overrides_host() {
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

    let raw = http_raw_bytes(
        srv.addr,
        b"GET http://target.example/echo?probe=target HTTP/1.1\r\n\
          Host: spoofed.example\r\nConnection: close\r\n\r\n",
        Duration::from_secs(10),
    )
    .expect("absolute-form request");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("target=http://target.example/echo?probe=target"),
        "the target must keep the absolute form: {text}\n{}",
        diagnostics(&srv)
    );
    assert!(
        text.contains("authority=target.example"),
        "the target authority must replace Host: {text}\n{}",
        diagnostics(&srv)
    );
    assert!(
        text.contains("uri=http://target.example/echo?probe=target"),
        "the absolute uri must carry the target authority and the path: {text}\n{}",
        diagnostics(&srv)
    );
    assert!(
        text.contains("host=target.example"),
        "the Host field line must carry the effective authority: {text}\n{}",
        diagnostics(&srv)
    );
}

/// Host-side multipart over the wire: a non-UTF-8 boundary round-trips, the spool file dies with finalization, malformed framing answers 400 and an over-limit file part 413.
#[test]
fn dispatcher_multipart_over_the_wire() {
    let srv = spawn_with_http_extra(
        "lifecycle/fidelity-worker.php",
        1,
        "[http.uploads]\nmax_file_size_mb = 1\n",
    );

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
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    let log = srv.log_file();
    tests::server_log::wait_app_record(&log, "booted");
    // max_requests = 1 is a quota of 2, so the slow second request ends the app instance.
    for path in ["/", "/?slow"] {
        let code = http_get(srv.addr, path, Duration::from_secs(10))
            .expect("GET")
            .status;
        assert_eq!(code, 200, "{path}\n{}", diagnostics(&srv));
    }
    let booted = poll(Duration::from_secs(10), || {
        (tests::server_log::app_contexts(&log, "booted").len() >= 2).then_some(())
    });
    assert!(
        booted.is_some(),
        "the app booted again\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        worker_pids(srv.pid()),
        vec![pid],
        "the watchdog did not kill the worker during the boot\n{}",
        diagnostics(&srv)
    );
}

/// SIGUSR2 to a worker whose app fails its boot boots the fixed app at once and logs no false "reload skipped" warning.
#[test]
fn a_reload_during_a_failing_boot_boots_the_fixed_app() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/breakable-boot-worker.php"),
    )
    .json_log()
    .spawn();
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    let log = srv.log_file();
    tests::server_log::wait_app_record(&log, "booted");
    let flag = srv.dir.join("http/broken.flag");
    std::fs::write(&flag, "").expect("break the boot");
    signal(pid, libc::SIGUSR2);
    assert!(
        wait_log_contains(&srv, "broken deploy", Duration::from_secs(10)),
        "no failed boot\n{}",
        diagnostics(&srv)
    );
    std::fs::remove_file(&flag).expect("fix the boot");
    signal(pid, libc::SIGUSR2);
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
