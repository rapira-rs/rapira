use std::{ops::Deref, path::Path};

use php_sys::{Frame, Mode, Rapira, Request};
use tests::{drain, fixture, php_lock, req};

fn post(fixture_name: &str, query: &str, content_type: Option<&str>, body: Vec<u8>) -> Request {
    let mut r: Request = req(&format!("/{fixture_name}?{query}"), fixture_name);
    r.method = "POST".into();
    r.content_type = content_type.map(|s| s.as_bytes().to_vec());
    r.content_length = body.len() as i64;
    r.body = Box::new(std::io::Cursor::new(body));
    r
}

/// Like `drain`, but keeps the head: (head presence, status, headers, body).
struct Resp {
    heads: u32,
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body: String,
}

fn recv_all(mut rx: tokio::sync::mpsc::Receiver<Frame>) -> Resp {
    let (mut heads, mut status, mut headers, mut body) = (0u32, 0u16, Vec::new(), String::new());
    if let Some(frame) = rx.blocking_recv() {
        if let Some(h) = frame.head {
            heads = 1;
            status = h.status;
            headers = h.headers;
        }
        body = String::from_utf8_lossy(&frame.body).into_owned();
    }
    Resp {
        heads,
        status,
        headers,
        body,
    }
}

fn header_value(r: &Resp, name: &str) -> Option<String> {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
}

// POST form body parses into $_POST while the query string populates $_GET.
#[test]
fn post_superglobals_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let request = post(
        "ported_tests/post-superglobals.php",
        "foo=bar&baz=buz",
        Some("application/x-www-form-urlencoded"),
        b"bam=bam&some=10".to_vec(),
    );
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    for expected in [
        "'foo' => 'bar'",
        "'baz' => 'buz'",
        "'bam' => 'bam'",
        "'some' => '10'",
    ] {
        assert!(
            body.contains(expected),
            "missing {expected:?} (got: {body:?})"
        );
    }
    Ok(())
}

// $_GET/$_POST must be rebuilt per worker request — no stale values.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn post_superglobals_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/post-superglobals-worker.php",
    )))?;
    let h = r.handle()?;
    let (s1, b1) = drain(h.handle_blocking(post(
        "ported_tests/post-superglobals-worker.php",
        "foo=bar&iG=42",
        Some("application/x-www-form-urlencoded"),
        b"baz=bat&i=7".to_vec(),
    ))?);
    let (s2, b2) = drain(h.handle_blocking(post(
        "ported_tests/post-superglobals-worker.php",
        "foo=bar&iG=43",
        Some("application/x-www-form-urlencoded"),
        b"baz=bat&i=8".to_vec(),
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 200);
    assert!(
        b1.contains("'iG' => '42'") && b1.contains("'i' => '7'"),
        "req1 (got: {b1:?})"
    );
    assert_eq!(s2, 200);
    assert!(
        b2.contains("'iG' => '43'") && b2.contains("'i' => '8'"),
        "req2 (got: {b2:?})"
    );
    assert!(
        !b2.contains("'42'") && !b2.contains("'7'"),
        "previous request's GET/POST must not leak (got: {b2:?})"
    );
    Ok(())
}

// $_REQUEST merges GET + POST under the default variables_order/request_order.
#[test]
fn request_merge_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(post(
        "ported_tests/request-merge.php",
        "get_key=get_value_1",
        Some("application/x-www-form-urlencoded"),
        b"post_key=post_value_1".to_vec(),
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    assert!(
        body.contains("'get_key' => 'get_value_1'")
            && body.contains("'post_key' => 'post_value_1'"),
        "$_REQUEST must merge GET and POST (got: {body:?})"
    );
    Ok(())
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn request_merge_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/request-merge-worker.php",
    )))?;
    let h = r.handle()?;
    for i in 1..=3 {
        let body_bytes = format!("post_key=post_value_{i}").into_bytes();
        let (status, body) = drain(h.handle_blocking(post(
            "ported_tests/request-merge-worker.php",
            &format!("get_key=get_value_{i}"),
            Some("application/x-www-form-urlencoded"),
            body_bytes,
        ))?);
        assert_eq!(status, 200);
        assert!(
            body.contains(&format!("'get_key' => 'get_value_{i}'"))
                && body.contains(&format!("'post_key' => 'post_value_{i}'")),
            "req{i}: $_REQUEST must carry only this request's data (got: {body:?})"
        );
    }
    drop(h);
    r.shutdown();
    Ok(())
}

// A jit autoglobal first touched only in a LATER request must still build fresh:
// req1 builds $_REQUEST, req2 never touches it, req3 must not see stale data.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn jit_request_superglobal_rearm_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/jit-request-worker.php",
    )))?;
    let h = r.handle()?;
    for i in 1..=4 {
        let query = if i % 2 == 1 {
            format!("use_request=1&val={i}")
        } else {
            format!("val={i}")
        };
        let (status, body) = drain(h.handle_blocking(req(
            &format!("/jit-request-worker.php?{query}"),
            "ported_tests/jit-request-worker.php",
        ))?);
        assert_eq!(status, 200);
        assert!(
            body.contains(&format!("'val' => '{i}'")),
            "req{i}: $_GET must be fresh (got: {body:?})"
        );
        if i % 2 == 1 {
            assert!(
                body.contains("REQUEST_COUNT:2") && body.contains("VAL_CHECK:MATCH"),
                "req{i}: $_REQUEST must rebuild from this request's data (got: {body:?})"
            );
            assert!(
                !body.contains("MISMATCH"),
                "req{i}: stale $_REQUEST (got: {body:?})"
            );
        } else {
            assert!(body.contains("SKIPPED"), "req{i} (got: {body:?})");
        }
    }
    drop(h);
    r.shutdown();
    Ok(())
}

// The Cookie header feeds $_COOKIE fresh on every worker request.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn cookies_refresh_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/cookies-worker.php",
    )))?;
    let h = r.handle()?;
    for i in 0..3 {
        let mut request = req("/cookies-worker.php", "ported_tests/cookies-worker.php");
        request
            .headers
            .push(("Cookie".into(), format!("foo=bar; i={i}").into_bytes()));
        let (status, body) = drain(h.handle_blocking(request)?);
        assert_eq!(status, 200);
        assert!(
            body.contains("'foo' => 'bar'") && body.contains(&format!("'i' => '{i}'")),
            "req{i}: $_COOKIE must reflect this request's header (got: {body:?})"
        );
    }
    drop(h);
    r.shutdown();
    Ok(())
}

// PHP's cookie parser mangles malformed names (spaces/dots -> underscores),
// drops separator-only segments, keeps trailing spaces in values, and keeps
// the FIRST occurrence of a duplicate name.
#[test]
fn malformed_cookies_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let mut request = req("/cookies.php", "ported_tests/cookies.php");
    request.headers.push((
        "Cookie".into(),
        "foo =bar; ===;;==;  .dot.=val  ; PHPSESSID=1234; dup=first; dup=second".into(),
    ));
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    for expected in [
        "'foo_' => 'bar'",
        "'_dot_' => 'val  '",
        "'PHPSESSID' => '1234'",
        "'dup' => 'first'",
        "count=4",
    ] {
        assert!(
            body.contains(expected),
            "missing {expected:?} (got: {body:?})"
        );
    }
    assert!(
        !body.contains("second"),
        "first duplicate must win (got: {body:?})"
    );
    Ok(())
}

fn session_roundtrip(mode: Mode, fixture_name: &str) -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(mode)?;
    let h = r.handle()?;

    let r1 = recv_all(h.handle_blocking(req(&format!("/{fixture_name}"), fixture_name))?);
    assert_eq!(r1.status, 200);
    assert_eq!(r1.body, "Count: 0\n", "fresh session starts at zero");
    let sid = r1
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .find_map(|(_, v)| {
            let s = String::from_utf8_lossy(v);
            s.strip_prefix("PHPSESSID=")
                .map(|rest| rest.split(';').next().unwrap_or(rest).trim().to_string())
        })
        .expect("session cookie must be issued");

    let mut request = req(&format!("/{fixture_name}"), fixture_name);
    request
        .headers
        .push(("Cookie".into(), format!("PHPSESSID={sid}").into_bytes()));
    let r2 = recv_all(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(r2.status, 200);
    assert_eq!(
        r2.body, "Count: 1\n",
        "returned cookie must resume the same session"
    );
    Ok(())
}

#[test]
fn session_cookie_roundtrip_classic() -> anyhow::Result<()> {
    session_roundtrip(Mode::Classic, "ported_tests/session-count.php")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn session_cookie_roundtrip_worker() -> anyhow::Result<()> {
    session_roundtrip(
        Mode::WorkerRequest(fixture("ported_tests/session-count-worker.php")),
        "ported_tests/session-count-worker.php",
    )
}

// A userland save handler registered DURING request 1 must still serve request 2.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn session_handler_registered_midstream_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/session-handler-worker.php",
    )))?;
    let h = r.handle()?;
    let (s1, b1) = drain(h.handle_blocking(req(
        "/session-handler-worker.php?action=register",
        "ported_tests/session-handler-worker.php",
    ))?);
    let (s2, b2) = drain(h.handle_blocking(req(
        "/session-handler-worker.php",
        "ported_tests/session-handler-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 200);
    assert!(
        b1.contains("REGISTERED save_handler=user"),
        "handler registration must flip session.save_handler (got: {b1:?})"
    );
    assert_eq!(s2, 200);
    assert!(
        b2.contains("START=true"),
        "second request must start a session (got: {b2:?})"
    );
    assert!(
        !b2.contains("ERROR:") && !b2.contains("EXCEPTION:"),
        "the registered handler must still be usable (got: {b2:?})"
    );
    Ok(())
}

// A save handler registered BEFORE the worker loop stays installed for all requests.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn session_preloop_handler_preserved_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/preloop-session-handler-worker.php",
    )))?;
    let h = r.handle()?;
    let (s1, b1) = drain(h.handle_blocking(req(
        "/preloop-session-handler-worker.php?action=check",
        "ported_tests/preloop-session-handler-worker.php",
    ))?);
    let (s2, b2) = drain(h.handle_blocking(req(
        "/preloop-session-handler-worker.php?action=use_session",
        "ported_tests/preloop-session-handler-worker.php",
    ))?);
    let (s3, b3) = drain(h.handle_blocking(req(
        "/preloop-session-handler-worker.php?action=check",
        "ported_tests/preloop-session-handler-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!((s1, s2, s3), (200, 200, 200));
    assert!(
        b1.contains("HANDLER_PRESERVED") && b1.contains("save_handler=user"),
        "req1 (got: {b1:?})"
    );
    assert!(
        b2.contains("SESSION_OK") && !b2.contains("ERROR:") && !b2.contains("EXCEPTION:"),
        "session must work through the pre-loop handler (got: {b2:?})"
    );
    assert!(
        b3.contains("HANDLER_PRESERVED"),
        "handler must survive a request that used the session (got: {b3:?})"
    );
    Ok(())
}

// header() edge cases: no-space colon is trimmed, a colon-less line never
// becomes a response header, the status set via http_response_code sticks,
// and the header set rebuilds per worker request.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn response_header_edges_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/headers-worker.php",
    )))?;
    let h = r.handle()?;
    for i in [42, 43] {
        let resp = recv_all(h.handle_blocking(req(
            &format!("/headers-worker.php?i={i}"),
            "ported_tests/headers-worker.php",
        ))?);
        assert_eq!(
            resp.status, 201,
            "http_response_code(201) must reach the head"
        );
        assert_eq!(header_value(&resp, "Foo").as_deref(), Some("bar"));
        assert_eq!(header_value(&resp, "Foo2").as_deref(), Some("bar2"));
        assert_eq!(
            header_value(&resp, "Foo3").as_deref(),
            Some("bar3"),
            "no-space colon must trim"
        );
        assert_eq!(
            header_value(&resp, "I").as_deref(),
            Some(format!("{i}").deref())
        );
        assert!(
            header_value(&resp, "Invalid").is_none(),
            "colon-less header line must not become a response header"
        );
        assert_eq!(resp.body, "Hello");
    }
    drop(h);
    r.shutdown();
    Ok(())
}

fn assert_headers_list_response(resp: &Resp, i: u16) {
    assert_eq!(resp.status, 200 + i);
    // the raw llist (headers_list dump in the body) keeps the colon-less line...
    for expected in ["X-Powered-By: PHP/", "Foo: bar", "Foo2: bar2", "Invalid"] {
        assert!(
            resp.body.contains(expected),
            "missing {expected:?} (got: {:?})",
            resp.body
        );
    }
    assert!(
        resp.body.contains(&format!("I: {i}")),
        "got: {:?}",
        resp.body
    );
    // ...while the head frame drops it and carries the parsed pairs
    assert_eq!(header_value(resp, "Foo").as_deref(), Some("bar"));
    assert!(
        header_value(resp, "X-Powered-By").is_some_and(|v| v.starts_with("PHP/")),
        "X-Powered-By must be a response header (headers: {:?})",
        resp.headers
    );
    assert!(header_value(resp, "Invalid").is_none());
}

#[test]
fn headers_list_and_expose_php_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let resp = recv_all(h.handle_blocking(req(
        "/response-headers.php?i=1",
        "ported_tests/response-headers.php",
    ))?);
    drop(h);
    r.shutdown();
    assert_headers_list_response(&resp, 1);
    Ok(())
}

// fail-first: worker requests must also carry the expose_php X-Powered-By
// header that a full per-request startup would add.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn headers_list_and_expose_php_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/response-headers-worker.php",
    )))?;
    let h = r.handle()?;
    for i in [1u16, 2] {
        let resp = recv_all(h.handle_blocking(req(
            &format!("/response-headers-worker.php?i={i}"),
            "ported_tests/response-headers-worker.php",
        ))?);
        assert_headers_list_response(&resp, i);
    }
    drop(h);
    r.shutdown();
    Ok(())
}

// Unbuffered output written across several ub_writes (with an explicit flush()
// between them) arrives whole and in order in the single sealed frame.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn flush_output_arrives_complete_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/flush-worker.php",
    )))?;
    let h = r.handle()?;
    for i in [42, 43] {
        let mut rx = h.handle_blocking(req(
            &format!("/flush-worker.php?i={i}"),
            "ported_tests/flush-worker.php",
        ))?;
        let frame = rx
            .blocking_recv()
            .expect("worker must seal exactly one frame");
        assert!(
            rx.blocking_recv().is_none(),
            "exactly one frame per response"
        );
        let head = frame.head.expect("head must be recorded");
        assert_eq!(head.status, 200);
        assert_eq!(
            &frame.body[..],
            format!("Hello {i}").as_bytes(),
            "flushed chunks arrive whole and in order"
        );
        assert!(!frame.truncated, "clean completion is not truncated");
    }
    drop(h);
    r.shutdown();
    Ok(())
}

// A raw status line — header('HTTP/1.1 204 No Content', true, 204) — drives the
// head status; the SAPI itself never suppresses the body for a 204.
#[test]
fn raw_status_line_204_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let resp =
        recv_all(h.handle_blocking(req("/only-headers.php", "ported_tests/only-headers.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(resp.heads, 1);
    assert_eq!(resp.status, 204);
    assert_eq!(
        header_value(&resp, "Content-Type").as_deref(),
        Some("application/json")
    );
    assert!(
        !resp.headers.iter().any(|(k, _)| k.starts_with("HTTP/")),
        "the raw status line must not appear as a header (headers: {:?})",
        resp.headers
    );
    assert_eq!(resp.body, r#"{"status": "test"}"#);
    Ok(())
}

// A 6MB body with no content type travels intact through php://input, and the
// next request's input is unaffected.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn large_post_body_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/large-request-worker.php",
    )))?;
    let h = r.handle()?;
    for _ in 0..2 {
        let (status, body) = drain(h.handle_blocking(post(
            "ported_tests/large-request-worker.php",
            "",
            None,
            vec![b'f'; 6_048_576],
        ))?);
        assert_eq!(status, 200);
        assert_eq!(body, "Request body size: 6048576");
    }
    drop(h);
    r.shutdown();
    Ok(())
}

fn multipart_body_with(boundary: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary);
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"file\"; filename=\"foo.txt\"\r\nContent-Type: text/plain\r\n\r\nbar\r\n--");
    body.extend_from_slice(boundary);
    body.extend_from_slice(b"--\r\n");
    body
}

fn multipart_body() -> Vec<u8> {
    multipart_body_with(b"RAPIRA")
}

fn assert_upload_and_cleanup(status: u16, body: &str) {
    assert_eq!(status, 200);
    let mut parts = body.splitn(4, '|');
    let (name, error, content, tmp) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    assert_eq!(name, "foo.txt", "got: {body:?}");
    assert_eq!(error, "0", "UPLOAD_ERR_OK expected (got: {body:?})");
    assert_eq!(
        content, "bar",
        "tmp file must hold the uploaded bytes (got: {body:?})"
    );
    assert!(
        !tmp.is_empty() && !Path::new(tmp).exists(),
        "upload tmp file must be deleted after the request (path: {tmp:?})"
    );
}

#[test]
fn multipart_upload_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(post(
        "ported_tests/upload.php",
        "",
        Some("multipart/form-data; boundary=RAPIRA"),
        multipart_body(),
    ))?);
    drop(h);
    r.shutdown();
    assert_upload_and_cleanup(status, &body);
    Ok(())
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn multipart_upload_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/upload-worker.php",
    )))?;
    let h = r.handle()?;
    for _ in 0..2 {
        let (status, body) = drain(h.handle_blocking(post(
            "ported_tests/upload-worker.php",
            "",
            Some("multipart/form-data; boundary=RAPIRA"),
            multipart_body(),
        ))?);
        assert_upload_and_cleanup(status, &body);
    }
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn files_superglobal_does_not_leak_between_worker_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/upload-worker.php",
    )))?;
    let h = r.handle()?;
    // $_FILES is the one superglobal whose create callback does not self-heal, so
    // rapira_reset_super_global dtors TRACK_VARS_FILES each request; without it a
    // no-upload request would re-expose the previous request's upload.
    let (s1, b1) = drain(h.handle_blocking(post(
        "ported_tests/upload-worker.php",
        "",
        Some("multipart/form-data; boundary=RAPIRA"),
        multipart_body(),
    ))?);
    assert_eq!(s1, 200);
    assert!(
        b1.starts_with("foo.txt|"),
        "req1 must see the upload (got {b1:?})"
    );

    let (s2, b2) =
        drain(h.handle_blocking(req("/upload-worker.php", "ported_tests/upload-worker.php"))?);
    assert_eq!(s2, 200);
    assert_eq!(
        b2, "NO FILE",
        "TRACK_VARS_FILES must reset; req2 must not see req1's upload (got {b2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

// Output already sent, then an uncaught throw: exactly one head, status 200
// (committed by the echo), the fatal text follows in the body, worker survives.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn uncaught_exception_after_output_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "shared/output-then-throw-worker.php",
    )))?;
    let h = r.handle()?;
    for i in [1, 2] {
        let resp = recv_all(h.handle_blocking(req(
            &format!("/output-then-throw-worker.php?i={i}"),
            "shared/output-then-throw-worker.php",
        ))?);
        assert_eq!(resp.heads, 1, "exactly one head frame (got {})", resp.heads);
        assert_eq!(resp.status, 200, "headers were committed by the echo");
        let hello = resp.body.find("hello");
        let uncaught = resp.body.find(&format!("Uncaught Exception: request {i}"));
        assert!(
            hello.is_some() && uncaught.is_some() && hello < uncaught,
            "echo output must precede the fatal text (got: {:?})",
            resp.body
        );
    }
    drop(h);
    r.shutdown();
    Ok(())
}

// Streams opened before the worker loop keep their identity and read position
// across requests — between-request cleanup must not touch live resources.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn preloop_streams_survive_requests_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/file-stream-worker.php",
    )))?;
    let h = r.handle()?;
    for expected in ["word1", "word2", "word3"] {
        let (status, body) = drain(h.handle_blocking(req(
            "/file-stream-worker.php",
            "ported_tests/file-stream-worker.php",
        ))?);
        assert_eq!(status, 200);
        assert_eq!(
            body, expected,
            "pre-loop stream must keep advancing cleanly"
        );
    }
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn error_path_keeps_status_and_cookies() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "shared/error-keeps-headers-worker.php",
    )))?;
    let h = r.handle()?;
    let resp = recv_all(h.handle_blocking(req("/", "shared/error-keeps-headers-worker.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(resp.heads, 1);
    assert_eq!(
        resp.status, 404,
        "script status must survive the fatal, not force 500"
    );
    assert!(
        resp.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("set-cookie")),
        "Set-Cookie must survive"
    );
    Ok(())
}

#[test]
fn multi_cookie_headers_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    // One entry per field name: repeats are combined by the front, so this is the shape
    // a request reaches php_sys in. Two wire Cookie headers are covered end to end by
    // the e2e suite.
    let mut request = req("/multi-cookie.php", "ported_tests/multi-cookie.php");
    request.headers.push(("Cookie".into(), "a=1; b=2".into()));
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();
    assert_eq!((status, body.as_str()), (200, "1,2,a=1; b=2"));
    Ok(())
}

#[test]
fn latin1_header_value_passes_through() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let resp = recv_all(h.handle_blocking(req("/", "ported_tests/latin1-header.php"))?);
    drop(h);
    r.shutdown();
    let v = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("X-Filename"))
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(v, b"caf\xE9.pdf".to_vec(), "0xE9 must not become U+FFFD");
    Ok(())
}

#[test]
fn error_path_keeps_status_and_cookies_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let resp = recv_all(h.handle_blocking(req(
        "/error-keeps-headers.php",
        "shared/error-keeps-headers.php",
    ))?);
    drop(h);
    r.shutdown();
    assert_eq!(resp.heads, 1, "exactly one head");
    assert_eq!(
        resp.status, 404,
        "script status must survive the fatal, not force 500"
    );
    assert!(
        resp.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("set-cookie")),
        "session Set-Cookie must reach the client (headers: {:?})",
        resp.headers
    );
    Ok(())
}

/// A boundary is opaque octets and obs-text is legal in a field value, so Content-Type
/// must reach php-src byte for byte — decoding it lossily leaves rfc1867 hunting for a
/// boundary the body never contains, and the upload silently vanishes.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn multipart_upload_non_utf8_boundary_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/upload-worker.php",
    )))?;
    let h = r.handle()?;
    let boundary: &[u8] = b"RAP\xff\xfeIRA";
    let mut request = post(
        "ported_tests/upload-worker.php",
        "",
        None,
        multipart_body_with(boundary),
    );
    let mut ctype = b"multipart/form-data; boundary=".to_vec();
    ctype.extend_from_slice(boundary);
    request.content_type = Some(ctype);
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();
    assert_upload_and_cleanup(status, &body);
    Ok(())
}

/// sapi_header_op screens only CR, LF and NUL, so a name with a space and a value with
/// a C0 control both reach the SAPI. Dropping those two fields must not cost the status,
/// the other headers, or the body.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn unrepresentable_header_does_not_sink_the_response_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "ported_tests/bad-header-worker.php",
    )))?;
    let h = r.handle()?;
    let resp = recv_all(h.handle_blocking(req(
        "/bad-header-worker.php",
        "ported_tests/bad-header-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!(resp.heads, 1, "a head must still be produced");
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body, "body");
    assert_eq!(header_value(&resp, "X-Keep").as_deref(), Some("kept"));
    assert!(header_value(&resp, "Content Type").is_none());
    assert!(header_value(&resp, "X-Ctl").is_none());
    Ok(())
}
