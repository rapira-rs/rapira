use crate::harness::*;
use std::time::{Duration, Instant};

#[test]
fn static_pool_forks_n_workers() {
    let srv = spawn_with_config("shared/echo-worker.php", 3, "");
    wait_workers(&srv, Duration::from_secs(20), "3 static workers", |p| {
        p.len() == 3
    });
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(
        code,
        200,
        "pool should serve once up\n{}",
        diagnostics(&srv)
    );
}

#[test]
fn http_round_trip() {
    let srv = spawn_with_config("shared/echo-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    for _ in 0..2 {
        let (code, body) =
            http_get(srv.addr, "/?from=e2e", Duration::from_secs(10)).expect("GET /?from=e2e");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
        assert!(
            body.starts_with(b"ok:"),
            "body should start with ok:, got {:?}",
            String::from_utf8_lossy(&body)
        );
    }
}

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
        let (code, body) = http_post(
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

        let (code, body) = http_get(srv.addr, "/export", timeout)
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
    for _ in 0..5 {
        let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
}

// After the master exits its workers reparent away and `worker_pids` cannot see them, so poll the captured pids directly.
fn wait_pids_gone(pids: &[u32], timeout: Duration, srv: &Server) {
    let end = Instant::now() + timeout;
    loop {
        // SAFETY: kill(pid, 0) only probes existence; ESRCH means gone.
        let gone = pids
            .iter()
            .all(|&p| unsafe { libc::kill(p as libc::pid_t, 0) } == -1);
        if gone {
            return;
        }
        assert!(
            Instant::now() < end,
            "workers survived the master: {pids:?}\n{}",
            diagnostics(srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// Must outlast supervisor.process_control_timeout (30s): after it the master escalates a stuck worker QUIT/TERM/KILL and still exits 0.
const STOP_BUDGET: Duration = Duration::from_secs(45);

#[test]
fn sigquit_master_graceful() {
    let mut srv = spawn_with_config("shared/echo-worker.php", 2, "");
    let pids = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
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

#[test]
fn max_requests_recycles() {
    let srv = spawn_with_config("shared/echo-worker.php", 1, "max_requests = 5\n");
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let pid0 = pids0[0];
    for _ in 0..40 {
        let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10))
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    http_get(srv.addr, "/", Duration::from_secs(10))
                } else {
                    Err(e)
                }
            })
            .expect("GET / (after one responseless-close retry)");
        assert_eq!(
            code,
            200,
            "request lost across a recycle\n{}",
            diagnostics(&srv)
        );
    }
    wait_workers(
        &srv,
        Duration::from_secs(30),
        "worker recycled to a new pid",
        |p| p.len() == 1 && p[0] != pid0,
    );
}

#[test]
fn request_timeout_kills_and_replaces_worker() {
    let srv = spawn_with_config(
        "lifecycle/hang-worker.php",
        1,
        "request_terminate_timeout_secs = 2\n",
    );
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
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
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET / after kill");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
}

#[test]
fn master_failboot_exits_70() {
    let mut srv = spawn_with_config("lifecycle/fatal-worker.php", 1, "");
    let addr = srv.addr;
    let end = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(st) = srv.try_status() {
            break Some(st);
        }
        if Instant::now() >= end {
            panic!("master never exited\n{}", diagnostics(&srv));
        }
        let _ = http_get(addr, "/", Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_exit_code(status, MASTER_EXIT_FAILBOOT, &srv);
}

/// A worker-mode bootstrap that never calls handle_request() must failboot the master, not hang or shed 503s forever.
#[test]
fn worker_bootstrap_that_never_serves_failboots() {
    let mut srv = spawn_with_config("lifecycle/never-loop-worker.php", 1, "mode = \"worker\"\n");
    let addr = srv.addr;
    let end = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(st) = srv.try_status() {
            break Some(st);
        }
        if Instant::now() >= end {
            panic!("master never exited\n{}", diagnostics(&srv));
        }
        let _ = http_get(addr, "/", Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_exit_code(status, MASTER_EXIT_FAILBOOT, &srv);
}

/// A client that walks away mid-handler must not take the worker down: the abort recycles the cycle and the next request is served.
#[test]
fn worker_survives_client_abandon() {
    let srv = spawn_with_config("lifecycle/hold-worker.php", 1, "mode = \"worker\"\n");
    let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
    c.send(b"GET / HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    assert!(
        wait_log_contains(&srv, "held", Duration::from_secs(10)),
        "\n{}",
        diagnostics(&srv)
    );
    c.abandon();

    let (code, body) = http_get(srv.addr, "/?probe=1", Duration::from_secs(10)).expect("GET");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(body, b"ok", "\n{}", diagnostics(&srv));
}

#[test]
fn observed_cancellation_allows_next_receive() {
    for method in ["receive", "tryReceive"] {
        let srv = spawn_with_config(
            "lifecycle/cancelled-receive-worker.php",
            1,
            "mode = \"dispatcher\"\n",
        );
        let pid = wait_workers(&srv, BOOT, "1 dispatcher worker", |p| p.len() == 1)[0];
        let mut client = Conn::open(srv.addr, BOOT).expect("connect");
        client
            .send(format!("GET /cancel?method={method} HTTP/1.1\r\nHost: e2e\r\n\r\n").as_bytes())
            .expect("send /cancel");
        let (status, fields) = client
            .read_head(BOOT)
            .unwrap_or_else(|e| panic!("/cancel head: {e}\n{}", diagnostics(&srv)));
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
            .read_body_until(format!("{pid}:1:initial\n").as_bytes(), BOOT)
            .unwrap_or_else(|e| panic!("/cancel body: {e}\n{}", diagnostics(&srv)));
        client.abandon();
        assert!(
            wait_log_contains(&srv, "cancellation-observed", BOOT),
            "PHP did not observe the client disconnect\n{}",
            diagnostics(&srv)
        );

        let mut next = Conn::open(srv.addr, BOOT).expect("connect /next");
        next.send(b"GET /next HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
            .expect("send /next");
        assert!(
            wait_log_contains(&srv, "receive-result:", BOOT),
            "{method}() did not return after cancellation\n{}",
            diagnostics(&srv)
        );
        assert!(
            !std::fs::read_to_string(srv.dir.join("server.log"))
                .expect("server.log")
                .contains("receive-result:error:"),
            "{method}() rejected observed cancellation\n{}",
            diagnostics(&srv)
        );
        let (status, _) = next
            .read_head(BOOT)
            .unwrap_or_else(|e| panic!("/next head: {e}\n{}", diagnostics(&srv)));
        assert_eq!(status, 200, "{method}()\n{}", diagnostics(&srv));
        let body = next
            .read_remaining(BOOT)
            .unwrap_or_else(|e| panic!("/next body: {e}\n{}", diagnostics(&srv)));
        assert_eq!(
            body,
            format!("{pid}:2").into_bytes(),
            "{method}() must preserve the worker and script cycle\n{}",
            diagnostics(&srv)
        );
    }
}

/// A field php-src lets through but no front can represent must cost only that field, not the response.
#[test]
fn unrepresentable_header_still_serves_the_response() {
    let srv = spawn_with_config("lifecycle/bad-header-worker.php", 1, "mode = \"worker\"\n");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let (code, body) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 201, "\n{}", diagnostics(&srv));
    assert_eq!(body, b"body", "\n{}", diagnostics(&srv));
}

/// The multipart boundary must reach php-src byte for byte: decoded lossily, rfc1867 searches for a boundary the body never contains and the upload silently vanishes.
#[test]
fn non_utf8_multipart_boundary_uploads() {
    let srv = spawn_with_config("lifecycle/upload-worker.php", 1, "mode = \"worker\"\n");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let boundary: &[u8] = b"RAP\xff\xfeIRA";
    let mut body = Vec::new();
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary);
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"file\"; filename=\"foo.txt\"\r\nContent-Type: text/plain\r\n\r\nbar\r\n--");
    body.extend_from_slice(boundary);
    body.extend_from_slice(b"--\r\n");
    let mut ctype = b"multipart/form-data; boundary=".to_vec();
    ctype.extend_from_slice(boundary);

    let (code, out) =
        http_post(srv.addr, "/", &ctype, &body, Duration::from_secs(10)).expect("POST /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    let out = String::from_utf8_lossy(&out);
    let tmp = out.strip_prefix("foo.txt|0|bar|").unwrap_or_else(|| {
        panic!("upload must parse (got {out:?})\n{}", diagnostics(&srv));
    });
    assert!(
        !std::path::Path::new(tmp).exists(),
        "upload temp file {tmp} must be cleaned up\n{}",
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
    let (code, body) = http_get_with_headers(
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
    let (code, body) = http_get_with_headers(
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

    let (code, _) = http_get_with_headers(
        srv.addr,
        "/",
        &[("X_Forwarded_For", "1.2.3.4")],
        Duration::from_secs(10),
    )
    .expect("GET / with an alias name");
    assert_eq!(code, 400, "\n{}", diagnostics(&srv));

    let (code, _) = http_get_with_headers(
        srv.addr,
        "/",
        &[("X-Forwarded-For", "203.0.113.7")],
        Duration::from_secs(10),
    )
    .expect("GET / with a safe name");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
}

/// More than one `Host` line is a 400; the h1 parser hands the repeated field through intact, so the rejection is this front's own.
/// RFC 9112 §3.2: https://www.rfc-editor.org/rfc/rfc9112#section-3.2
#[test]
fn a_second_host_field_line_answers_400() {
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

    let (code, _) = http_get_with_headers(
        srv.addr,
        "/",
        &[("Host", "evil.example")],
        Duration::from_secs(10),
    )
    .expect("GET / with two Host field lines");
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
    let (status, _) = c.read_head(Duration::from_secs(10)).expect("413 head");
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
    let (status, _) = c.read_head(Duration::from_secs(10)).expect("head");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    c.read_body_until(b"ok", Duration::from_secs(10))
        .expect("body");
}

/// The interim 100 goes out when the body is first polled, which happens only after admission.
#[test]
fn expect_100_continue_gets_the_interim_then_the_response() {
    let srv = spawn_with_config("lifecycle/fidelity-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

    let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
    c.send(b"POST / HTTP/1.1\r\nHost: e2e\r\nExpect: 100-continue\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
        .expect("head");
    let (status, _) = c.read_head(Duration::from_secs(10)).expect("interim head");
    assert_eq!(status, 100, "\n{}", diagnostics(&srv));
    c.send(b"hello").expect("body");
    let (status, _) = c.read_head(Duration::from_secs(10)).expect("final head");
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
    let (status, _) = c
        .read_head(Duration::from_secs(10))
        .expect("direct refusal");
    assert_eq!(
        status,
        413,
        "no interim before the refusal\n{}",
        diagnostics(&srv)
    );
}

/// A final 1xx from PHP cannot go on the wire: hyper would rewrite it to a 500 and error the connection, so the front serves 502.
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
    let (code, body) = http_get_with_headers(
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

    let (code, body) =
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

    let (code, _) = http_raw(
        srv.addr,
        b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n",
        Duration::from_secs(10),
    )
    .expect("Host-less request");
    assert_eq!(code, 400, "missing Host on HTTP/1.1 must answer 400");
}

/// A PHP-written head crosses the wire: the status line, one field line per list value, the front's own framing, and a HEAD carrying neither body nor content-length.
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

    let raw = http_raw_bytes(
        srv.addr,
        b"HEAD /?probe=head HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        Duration::from_secs(10),
    )
    .expect("HEAD head probe");
    let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    assert!(
        text.starts_with("http/1.1 201"),
        "got {:?}\n{}",
        text.lines().next().unwrap_or(""),
        diagnostics(&srv)
    );
    assert!(
        !text.contains("\r\ncontent-length:"),
        "no content-length on a HEAD response\n{}",
        diagnostics(&srv)
    );
    assert!(
        text.ends_with("\r\n\r\n"),
        "no body bytes on a HEAD response\n{}",
        diagnostics(&srv)
    );
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
    let (code, resp) = http_post(
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
    assert!(
        !std::path::Path::new(tmp).exists(),
        "spool file must be gone once the response arrived"
    );

    let (code, _) = http_post(
        srv.addr,
        "/?probe=multipart",
        b"multipart/form-data; boundary=B",
        b"no boundary line at all",
        Duration::from_secs(10),
    )
    .expect("malformed POST");
    assert_eq!(code, 400, "malformed multipart must answer 400");

    let mut big = Vec::new();
    big.extend_from_slice(
        b"--B\r\ncontent-disposition: form-data; name=\"f\"; filename=\"a\"\r\n\r\n",
    );
    big.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
    big.extend_from_slice(b"\r\n--B--");
    let (code, _) = http_post(
        srv.addr,
        "/?probe=multipart",
        b"multipart/form-data; boundary=B",
        &big,
        Duration::from_secs(10),
    )
    .expect("over-limit POST");
    assert_eq!(code, 413, "over-limit file part must answer 413");
}
