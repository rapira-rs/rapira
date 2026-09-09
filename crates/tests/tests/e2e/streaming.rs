use std::time::{Duration, Instant};

use crate::harness::{
    Conn, decode_chunked, diagnostics, http_get_raw, spawn_with_config, wait_log_contains,
    wait_workers,
};

const T: Duration = Duration::from_secs(10);

/// `flush()` puts the head on the wire ahead of the 400ms-late first event.
#[test]
fn sse_head_and_events_reach_the_wire_incrementally() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=sse HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");

    let started = Instant::now();
    let (status, fields) = c.read_head(T).expect("flushed head");
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
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");

    c.send(b"GET /?probe=chunks HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let (status, fields) = c.read_head(T).expect("head");
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
    let (status, _) = c.read_head(T).expect("reused connection must serve");
    assert_eq!(status, 200);
}

/// A PHP-committed 1xx never reaches the wire: the front drops interim heads, so the first head block is the final 200 and the response is otherwise untouched.
#[test]
fn interim_heads_never_reach_the_wire() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=interim HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");

    let (status, _) = c.read_head(T).expect("final head");
    assert_eq!(status, 200, "the first head block must be the final one");
    c.read_body_until(b"hello", T).expect("body");
}

/// An HTTP/1.0 client gets close-delimited framing, never chunked.
#[test]
fn http10_gets_no_interim_and_no_chunked() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");

    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=interim HTTP/1.0\r\n\r\n")
        .expect("send");
    let (status, fields) = c.read_head(T).expect("head");
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
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=cl-slow-body HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");

    let started = Instant::now();
    let (status, fields) = c.read_head(T).expect("head");
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

fn assert_completed_response_finalizes(path: &str, contents: &[u8]) {
    for close in [false, true] {
        let srv = spawn_with_config(
            "lifecycle/completed-response-worker.php",
            1,
            "mode = \"dispatcher\"\n",
        );
        let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
        if path == "/file" {
            std::fs::write(srv.dir.join("payload.bin"), contents).expect("write payload");
        }
        let mut c = Conn::open(srv.addr, T).expect("connect");
        let connection = if close { "Connection: close\r\n" } else { "" };
        c.send(format!("GET {path} HTTP/1.1\r\nHost: e2e\r\n{connection}\r\n").as_bytes())
            .expect("send");
        let (status, fields) = c.read_head(T).expect("head");
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
        std::fs::write(srv.dir.join("client-read"), b"read").expect("release PHP");
        c.send(b"GET /state HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
            .expect("next request");
        let (status, _) = c.read_head(T).expect("next head");
        assert_eq!(status, 200, "\n{}", diagnostics(&srv));
        assert_eq!(
            c.read_remaining(T).expect("finalization result"),
            format!("{pid}:2:[false,true,false]").as_bytes(),
            "{path}, close={close}: complete delivery must preserve PHP finalization\n{}",
            diagnostics(&srv)
        );
    }
}

/// An over-long body is cut to the declared content-length, keeping framing keepalive-safe.
#[test]
fn content_length_exceeded_serves_the_declared_prefix() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let raw = http_get_raw(srv.addr, "/?probe=cl-exceeded", &[], T).expect("response");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.to_ascii_lowercase().contains("content-length: 5"),
        "declared length must be honoured: {text}"
    );
    assert!(
        text.ends_with("\r\n\r\n01234"),
        "exactly the fitting prefix: {text}"
    );
}

/// A client that walks away mid-stream surfaces as WorkDiscardedException in the worker.
#[test]
fn client_abort_discards_the_unit() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=discard HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let (status, _) = c.read_head(T).expect("flushed head");
    assert_eq!(status, 200);
    c.abandon();

    assert!(
        wait_log_contains(&srv, "WorkDiscardedException", T),
        "the worker must observe the abort on its next write"
    );
}

/// A worker dying mid-stream must not fake a clean end: no chunked terminator arrives.
#[test]
fn worker_death_mid_stream_truncates_the_response() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=die-mid-stream HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let (status, _) = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    c.read_body_until(b"first,", T).expect("first chunk");
    let rest = c.read_remaining(T).expect("connection drops");
    let full = [b"6\r\nfirst,\r\n".to_vec(), rest].concat();
    assert!(
        decode_chunked(&full).is_err(),
        "no clean terminator after a mid-stream death"
    );
}

/// sendFile streams the file from disk with a real content-length; PHP never holds the bytes.
#[test]
fn sendfile_streams_from_disk_with_length() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let payload = srv.dir.join("payload.bin");
    std::fs::write(&payload, vec![b'z'; 100_000]).expect("payload");

    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(
        format!(
            "GET /?probe=sendfile HTTP/1.1\r\nHost: e2e\r\nx-path: {}\r\nConnection: close\r\n\r\n",
            payload.display()
        )
        .as_bytes(),
    )
    .expect("send");
    let (status, fields) = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "content-length" && v == "100000"),
        "the file length is known up front: {fields:?}"
    );
    let rest = c.read_remaining(T).expect("body");
    assert_eq!(rest.len(), 100_000);
    assert!(rest.iter().all(|&b| b == b'z'), "file bytes intact");
}

/// Trailers are dropped on h1: no trailer bytes in the epilogue, connection stays reusable.
#[test]
fn trailers_are_dropped_on_h1_and_the_response_ends_cleanly() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=trailers HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");

    let (status, fields) = c.read_head(T).expect("head");
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
    let (status, _) = c.read_head(T).expect("reused connection must serve");
    assert_eq!(status, 200);
}
