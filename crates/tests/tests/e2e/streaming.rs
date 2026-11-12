use std::time::{Duration, Instant};

use rapira_sapi::Frame;
use rapira_sapi::plugin::Mode;
use tests::wire::submit;
use tests::{Resp, StatusBody, drain_resp_deadline, poll, req, server_log};
use tokio::sync::mpsc::Receiver;

use crate::harness::{
    Conn, Head, Server, Spawn, diagnostics, fixture_path, http_get, http_get_raw,
    wait_log_contains, wait_workers, worker_pids,
};

const T: Duration = Duration::from_secs(10);

/// `flush()` puts the head on the wire ahead of the 400ms-late first event.
#[test]
fn sse_head_and_events_reach_the_wire_incrementally() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=sse HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");

    let started = Instant::now();
    let Head { status, fields } = c.read_head(T).expect("flushed head");
    assert_eq!(status, 200);
    assert!(
        started.elapsed() < Duration::from_millis(350),
        "the head must beat the 400ms-late first event (took {:?})",
        started.elapsed()
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v == "chunked"),
        "flush costs the length: chunked framing expected, got {fields:?}"
    );

    c.read_body_until(b"data: one", T).expect("first event");
    c.read_body_until(b"data: two", T).expect("second event");
    let rest = c.read_remaining(T).expect("clean end");
    assert!(
        String::from_utf8_lossy(&rest).ends_with("0\r\n\r\n"),
        "the chunked terminator must close the stream"
    );
}

/// A chunked stream keeps the connection reusable for a second request.
#[test]
fn chunked_stream_preserves_keepalive() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    let mut c = Conn::open(srv.addr, T).expect("connect");

    c.send(b"GET /?probe=chunks HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let Head { status, fields } = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v == "chunked"),
        "{fields:?}"
    );
    c.read_body_until(b"0\r\n\r\n", T).expect("terminator");

    c.send(b"GET /?probe=chunks HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("second request on the same connection");
    let status = c.read_head(T).expect("reused connection must serve").status;
    assert_eq!(status, 200);
}

/// A PHP-committed 1xx never reaches the wire: the plugin drops interim heads, so the first head block is the final 200 and the response is otherwise untouched.
#[test]
fn interim_heads_never_reach_the_wire() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=interim HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");

    let status = c.read_head(T).expect("final head").status;
    assert_eq!(status, 200, "the first head block must be the final one");
    c.read_body_until(b"hello", T).expect("body");
}

/// An HTTP/1.0 client gets close-delimited framing, never chunked.
#[test]
fn http10_gets_no_interim_and_no_chunked() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();

    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=interim HTTP/1.0\r\n\r\n")
        .expect("send");
    let Head { status, fields } = c.read_head(T).expect("head");
    assert_eq!(status, 200, "the interim head must be dropped for 1.0");
    assert!(
        !fields.iter().any(|(k, _)| k == "transfer-encoding"),
        "no chunked toward a 1.0 client: {fields:?}"
    );
    let rest = c.read_remaining(T).expect("close-delimited body");
    assert_eq!(rest, b"hello");
}

/// A declared length must not hold the head hostage: the head+first-chunk coalescing window is bounded.
#[test]
fn declared_length_head_beats_a_slow_body() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=cl-slow-body HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");

    let started = Instant::now();
    let Head { status, fields } = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "the head must beat the 2s-late body (took {:?})",
        started.elapsed()
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "content-length" && v == "5"),
        "the declared length still frames the response: {fields:?}"
    );
    c.read_body_until(b"01234", T).expect("body");
}

#[test]
fn fixed_length_completion_does_not_cancel_php() {
    assert_completed_response_finalizes("/body", b"01234");
}

#[test]
fn empty_fixed_length_completion_does_not_cancel_php() {
    assert_completed_response_finalizes("/empty", b"");
}

#[test]
fn file_completion_does_not_cancel_php() {
    assert_completed_response_finalizes("/file", &vec![b'f'; 100_000]);
}

/// A HEAD reply is complete once its head is flushed: a close after that must not cancel PHP.
#[test]
fn head_completion_does_not_cancel_php() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/completed-response-worker.php"),
    )
    .spawn();
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"HEAD /body HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");
    let status = c.read_head(T).expect("head").status;
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    assert!(
        c.read_remaining(T).expect("response closed").is_empty(),
        "no body bytes on a HEAD response\n{}",
        diagnostics(&srv)
    );

    std::fs::write(srv.dir.join("http/client-read"), b"read").expect("release PHP");
    let StatusBody { status, body } = http_get(srv.addr, "/state", T)
        .unwrap_or_else(|e| panic!("GET /state: {e}\n{}", diagnostics(&srv)));
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        body,
        format!("{pid}:2:finalized").as_bytes(),
        "a delivered head must leave PHP free to finalize\n{}",
        diagnostics(&srv)
    );
}

fn assert_completed_response_finalizes(path: &str, contents: &[u8]) {
    for close in [false, true] {
        let srv = Spawn::http(
            Mode::Dispatcher,
            fixture_path("lifecycle/completed-response-worker.php"),
        )
        .spawn();
        let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
        if path == "/file" {
            std::fs::write(srv.dir.join("http/payload.bin"), contents).expect("write payload");
        }
        let mut c = Conn::open(srv.addr, T).expect("connect");
        let connection = if close { "Connection: close\r\n" } else { "" };
        c.send(format!("GET {path} HTTP/1.1\r\nHost: e2e\r\n{connection}\r\n").as_bytes())
            .expect("send");
        let Head { status, fields } = c.read_head(T).expect("head");
        assert_eq!(status, 200, "\n{}", diagnostics(&srv));
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "content-length" && v == &contents.len().to_string()),
            "{path}, close={close}: {fields:?}"
        );
        assert_eq!(
            c.read_n(contents.len(), T).expect("complete body"),
            contents
        );
        if close {
            assert!(c.read_remaining(T).expect("response closed").is_empty());
            c = Conn::open(srv.addr, T).expect("next connection");
        }

        // Let PHP finalize only after the client receives the complete response.
        std::fs::write(srv.dir.join("http/client-read"), b"read").expect("release PHP");
        c.send(b"GET /state HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
            .expect("next request");
        let status = c.read_head(T).expect("next head").status;
        assert_eq!(status, 200, "\n{}", diagnostics(&srv));
        assert_eq!(
            c.read_remaining(T).expect("finalization result"),
            format!("{pid}:2:finalized").as_bytes(),
            "{path}, close={close}: complete delivery must preserve PHP finalization\n{}",
            diagnostics(&srv)
        );
    }
}

#[test]
fn later_connection_error_does_not_cancel_completed_response() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/completed-response-worker.php"),
    )
    .spawn();
    let pid = wait_workers(&srv, T, "1 worker", |p| p.len() == 1)[0];
    let mut client = Conn::open(srv.addr, T).expect("connect");
    client
        .send(b"GET /body HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send first request");
    let status = client.read_head(T).expect("head").status;
    assert_eq!(status, 200);
    assert_eq!(client.read_n(5, T).expect("complete body"), b"01234");

    client
        .send(b"GET / HTTP/1.1\r\nHost: e2e\r\ninvalid\r\n\r\n")
        .expect("send malformed next request");
    let status = client.read_head(T).expect("error response").status;
    assert_eq!(status, 400);
    client.read_remaining(T).expect("failed connection closes");

    std::fs::write(srv.dir.join("http/client-read"), b"read").expect("release PHP");
    let StatusBody { status, body } = http_get(srv.addr, "/state", T)
        .unwrap_or_else(|e| panic!("GET /state: {e}\n{}", diagnostics(&srv)));
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        body,
        format!("{pid}:2:finalized").as_bytes(),
        "\n{}",
        diagnostics(&srv)
    );
}

#[test]
fn reset_during_buffered_write_cancels_php() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::os::fd::AsRawFd;

    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/buffered-download-worker.php"),
    )
    .spawn();
    let pid = wait_workers(&srv, T, "1 worker", |p| p.len() == 1)[0];
    let mut client = TcpStream::connect_timeout(&srv.addr, T).expect("connect");
    client.set_read_timeout(Some(T)).expect("read timeout");
    client.set_write_timeout(Some(T)).expect("write timeout");
    let receive_buffer: libc::c_int = 4096;
    // SAFETY: client owns the descriptor; the option pointer and size match c_int.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                client.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&receive_buffer as *const libc::c_int).cast(),
                std::mem::size_of_val(&receive_buffer) as libc::socklen_t,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    client
        .write_all(b"GET /download HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send download request");
    let mut prefix = [0; 4096];
    client.read_exact(&mut prefix).expect("response prefix");
    assert!(prefix.starts_with(b"HTTP/1.1 200 "));
    let head_end = prefix
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head");
    assert!(head_end + 4 < prefix.len(), "read body bytes before reset");
    assert!(
        String::from_utf8_lossy(&prefix[..head_end])
            .to_ascii_lowercase()
            .contains("content-length: 33554432")
    );
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: client owns the descriptor; the option pointer and size match linger.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                client.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&linger as *const libc::linger).cast(),
                std::mem::size_of_val(&linger) as libc::socklen_t,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    drop(client);

    let StatusBody { status, body } = http_get(srv.addr, "/state", T)
        .unwrap_or_else(|e| panic!("GET /state after reset: {e}\n{}", diagnostics(&srv)));
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        body,
        format!("{pid}:2:discarded").as_bytes(),
        "\n{}",
        diagnostics(&srv)
    );
}

/// An over-long body is cut to the declared content-length, keeping framing keepalive-safe. A piece of 16 KiB or more is cut in place, a smaller piece is copied.
#[test]
fn content_length_exceeded_serves_the_declared_prefix() {
    struct Case {
        name: &'static str,
        declared: usize,
        /// Copies of "0123456789" in the piece.
        tens: usize,
    }
    let cases = [
        Case {
            name: "a copied piece of 10 bytes",
            declared: 5,
            tens: 1,
        },
        Case {
            name: "a piece of 20480 bytes that the IO thread reads in place",
            declared: 20475,
            tens: 2048,
        },
    ];
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    for case in cases {
        let target = format!(
            "/?probe=cl-exceeded&tens={}&cl={}",
            case.tens, case.declared
        );
        let raw = http_get_raw(srv.addr, &target, &[], T).expect("response");
        let text = String::from_utf8_lossy(&raw);
        let prefix = &"0123456789".repeat(case.tens)[..case.declared];
        assert!(
            text.to_ascii_lowercase()
                .contains(&format!("content-length: {}", case.declared)),
            "{}: declared length must be honoured: {text}",
            case.name
        );
        assert!(
            text.ends_with(&format!("\r\n\r\n{prefix}")),
            "{}: exactly the fitting prefix: {text}",
            case.name
        );
    }
}

/// A client that walks away mid-stream surfaces as WorkDiscardedException in the worker.
#[test]
fn client_abort_discards_the_unit() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=discard HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let status = c.read_head(T).expect("flushed head").status;
    assert_eq!(status, 200);
    c.leave();

    assert!(
        wait_log_contains(&srv, "WorkDiscardedException", T),
        "the worker must observe the abort on its next write"
    );
}

/// A worker dying mid-stream must not fake a clean end: no chunked terminator arrives.
#[test]
fn worker_death_mid_stream_truncates_the_response() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=die-mid-stream HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let status = c.read_head(T).expect("head").status;
    assert_eq!(status, 200);
    c.read_body_until(b"first,", T).expect("first chunk");
    let rest = c.read_remaining(T).expect("connection drops");
    assert!(
        !rest.ends_with(b"0\r\n\r\n"),
        "no clean terminator after a mid-stream death: {:?}",
        String::from_utf8_lossy(&rest)
    );
}

/// sendFile streams the file from disk with a real content-length; PHP never holds the bytes.
#[test]
fn sendfile_streams_from_disk_with_length() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    let payload = srv.dir.join("http/payload.bin");
    std::fs::write(&payload, vec![b'z'; 600_000]).expect("payload");

    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(
        format!(
            "GET /?probe=sendfile HTTP/1.1\r\nHost: e2e\r\nx-path: {}\r\nConnection: close\r\n\r\n",
            payload.display()
        )
        .as_bytes(),
    )
    .expect("send");
    let Head { status, fields } = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "content-length" && v == "600000"),
        "the file length is known up front: {fields:?}"
    );
    let rest = c.read_remaining(T).expect("body");
    assert_eq!(rest.len(), 600_000);
    assert!(rest.iter().all(|&b| b == b'z'), "file bytes intact");
}

/// Trailers are dropped on h1: no trailer bytes in the epilogue, connection stays reusable.
#[test]
fn trailers_are_dropped_on_h1_and_the_response_ends_cleanly() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/stream-worker.php"),
    )
    .spawn();
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=trailers HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");

    let Head { status, fields } = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v == "chunked"),
        "{fields:?}"
    );
    c.read_body_until(b"payload", T).expect("body");
    c.read_body_until(b"0\r\n\r\n", T)
        .expect("a clean terminator with no trailer section");

    c.send(b"GET /?probe=chunks HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("second request");
    let status = c.read_head(T).expect("reused connection must serve").status;
    assert_eq!(status, 200);
}

/// The instance id that `lifecycle/large-body-worker.php` answers on a target with no probe.
fn instance(srv: &Server) -> String {
    let mut rx = submit(srv.addr, req("/")).expect("connect");
    let resp = drain_resp_deadline(&mut rx, Instant::now() + T).expect("a reply in time");
    assert_eq!(resp.status(), 200, "\n{}", diagnostics(srv));
    resp.body_string()
}

/// The instance ids of the `booted` records, in log order, once the log holds `count` of them.
fn booted(srv: &Server, count: usize) -> Vec<String> {
    let ids = || -> Vec<String> {
        server_log::app_contexts(&srv.log_file(), "booted")
            .iter()
            .filter_map(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .map(|ctx| ctx["instance"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    poll(T, || Some(ids()).filter(|ids| ids.len() >= count)).unwrap_or_else(ids)
}

/// The response of `rx`, read with a pause after each frame.
fn read_slowly(mut rx: Receiver<Frame>) -> Resp {
    let mut resp = Resp::default();
    while let Some(frame) = rx.blocking_recv() {
        if resp.fold(frame) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    resp
}

/// 160 writes of one 64 KiB string, read by a client that pauses after each frame. max_requests = 1 is a quota of 2, so the app instance ends after the large unit, while the IO thread still holds the pieces. Every byte read repeats the id of the instance that wrote it, the worker process stays, and a new app instance serves the next request on a new connection.
#[test]
fn a_slow_client_reads_the_pieces_of_an_app_instance_that_ended() {
    struct Case {
        name: &'static str,
        probe: &'static str,
        truncated: bool,
        /// The text of the php record that the probe causes.
        php_record: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "the whole 10 MiB body, then the recycle",
            probe: "large",
            truncated: false,
            php_record: None,
        },
        Case {
            name: "a fatal error after the pieces cuts the body",
            probe: "fatal",
            truncated: true,
            php_record: Some("Call to undefined function rapira_e2e_undefined_function()"),
        },
    ];
    for case in cases {
        let srv = Spawn::http(
            Mode::Dispatcher,
            fixture_path("lifecycle/large-body-worker.php"),
        )
        .http_pool("max_requests = 1")
        .json_log()
        .spawn();
        let pid = wait_workers(&srv, T, "1 worker", |p| p.len() == 1)[0];
        let first = instance(&srv);
        let read = read_slowly(
            submit(srv.addr, req(&format!("/?probe={}", case.probe))).expect("connect"),
        );
        let whole = first.repeat(4096 * 160).into_bytes();
        assert_eq!(read.status(), 200, "{}\n{}", case.name, diagnostics(&srv));
        assert_eq!(
            read.truncated || !read.ended,
            case.truncated,
            "{}: the body ended with an error\n{}",
            case.name,
            diagnostics(&srv)
        );
        let first_mismatch = read
            .body
            .iter()
            .zip(&whole)
            .position(|(got, want)| got != want);
        assert_eq!(
            first_mismatch,
            None,
            "{}: every byte repeats the id {first}; the body from the mismatch: {:?}",
            case.name,
            first_mismatch
                .map(|at| String::from_utf8_lossy(&read.body[at..read.body.len().min(at + 48)]))
        );
        // A cut body can lose the bytes that hyper had queued when the body ended with an error, and the byte check above sees only what arrived.
        let least = if case.truncated { 65536 } else { whole.len() };
        assert!(
            read.body.len() >= least,
            "{}: {} of at least {least} body bytes\n{}",
            case.name,
            read.body.len(),
            diagnostics(&srv)
        );
        assert_eq!(
            worker_pids(srv.pid()),
            [pid],
            "{}: the worker process stays\n{}",
            case.name,
            diagnostics(&srv)
        );
        let next = instance(&srv);
        assert_ne!(
            next,
            first,
            "{}: a new app instance serves the next request\n{}",
            case.name,
            diagnostics(&srv)
        );
        assert_eq!(
            booted(&srv, 2),
            [first, next],
            "{}: one boot record per app instance",
            case.name
        );
        if let Some(text) = case.php_record {
            let logged = poll(T, || {
                server_log::app_records(&srv.log_file())
                    .php_messages
                    .iter()
                    .any(|m| m.contains(text))
                    .then_some(())
            });
            assert!(
                logged.is_some(),
                "{}: a php record with {text:?}\n{}",
                case.name,
                diagnostics(&srv)
            );
        }
    }
}

/// A client that reads nothing after the head: the write timeout closes the connection, and the cut body has no chunked terminator. max_requests = 1 is a quota of 2, so the app instance ends after the large unit, and the next boot comes after the cut.
/// With 64 KiB pieces, the PHP thread waits on the full frame channel until the cut. The 1 MiB pieces fit in the channel, and the IO thread holds the first 8 of them in place (the 8 MiB cap), so the cycle end waits for the cut.
#[test]
fn the_write_timeout_cuts_a_stalled_client_before_the_app_instance_ends() {
    struct Case {
        name: &'static str,
        probe: &'static str,
    }
    let cases = [
        Case {
            name: "160 pieces of 64 KiB: the PHP thread waits on the full channel",
            probe: "large",
        },
        Case {
            name: "12 pieces of 1 MiB: the cycle end waits for the cut",
            probe: "stall",
        },
    ];
    for case in cases {
        let srv = Spawn::http(
            Mode::Dispatcher,
            fixture_path("lifecycle/large-body-worker.php"),
        )
        .http_pool("max_requests = 1")
        .http_extra("write_timeout_secs = 2")
        .rust_log("info,http=debug")
        .json_log()
        .spawn();
        let first = instance(&srv);
        let mut c = Conn::open(srv.addr, T).expect("connect");
        c.send(format!("GET /?probe={} HTTP/1.1\r\nHost: e2e\r\n\r\n", case.probe).as_bytes())
            .expect("send");
        assert_eq!(c.read_head(T).expect("head").status, 200, "{}", case.name);
        assert!(
            wait_log_contains(&srv, "response write timed out", T),
            "{}: the write timeout closes the connection\n{}",
            case.name,
            diagnostics(&srv)
        );
        let rest = c
            .read_remaining(Duration::from_secs(30))
            .unwrap_or_else(|e| {
                panic!(
                    "{}: the server closed the connection: {e}\n{}",
                    case.name,
                    diagnostics(&srv)
                )
            });
        assert!(
            !rest.ends_with(b"0\r\n\r\n"),
            "{}: the cut body has no chunked terminator",
            case.name
        );
        assert_ne!(
            instance(&srv),
            first,
            "{}: a new app instance serves the next request\n{}",
            case.name,
            diagnostics(&srv)
        );
        booted(&srv, 2);
        let log = server_log::records(&srv.log_file());
        let timed_out = log
            .iter()
            .position(|r| r.message.starts_with("response write timed out"));
        let next_boot = log
            .iter()
            .enumerate()
            .filter(|(_, r)| r.target == "app" && r.message == "booted")
            .nth(1)
            .map(|(at, _)| at);
        assert!(
            matches!((timed_out, next_boot), (Some(cut), Some(boot)) if cut < boot),
            "{}: the next boot comes after the cut (record positions {timed_out:?}, {next_boot:?})\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
}

/// With `keepalive_timeout_secs`, the server answers 408 to a body that stops, and closes a keep-alive connection that stays idle. Each timer starts after the request is sent, so the close comes no earlier than the timeout. The 10 s reads leave 9 s for a slow runner; the 60 s default and the 30 s hyper default miss them.
#[test]
fn the_keepalive_timeout_ends_a_stalled_body_and_an_idle_connection() {
    const KEEPALIVE: Duration = Duration::from_secs(1);
    struct Case {
        name: &'static str,
        request: &'static [u8],
        status: u16,
    }
    let cases = [
        Case {
            name: "a body that stops after 1 of 10 bytes",
            request: b"POST / HTTP/1.1\r\nHost: e2e\r\nContent-Length: 10\r\n\r\nx",
            status: 408,
        },
        Case {
            name: "an idle keep-alive connection after a response",
            request: b"GET / HTTP/1.1\r\nHost: e2e\r\n\r\n",
            status: 200,
        },
    ];
    let srv = Spawn::http(Mode::Dispatcher, fixture_path("shared/echo-worker.php"))
        .http_extra(&format!("keepalive_timeout_secs = {}", KEEPALIVE.as_secs()))
        .spawn();
    for case in cases {
        let mut c = Conn::open(srv.addr, T).expect("connect");
        let sent = Instant::now();
        c.send(case.request).expect("send");
        let head = c
            .read_head(T)
            .unwrap_or_else(|e| panic!("{}: {e}\n{}", case.name, diagnostics(&srv)));
        assert_eq!(head.status, case.status, "{}", case.name);
        c.read_remaining(T).unwrap_or_else(|e| {
            panic!(
                "{}: the server closes the connection: {e}\n{}",
                case.name,
                diagnostics(&srv)
            )
        });
        let closed = sent.elapsed();
        assert!(
            closed >= KEEPALIVE,
            "{}: closed after {closed:?}, before the timeout",
            case.name
        );
    }
}
