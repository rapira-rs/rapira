use std::{ops::Deref, path::Path};

use php_sys::{Mode, Rapira, Request};
use tests::{Resp, captured, drain, drain_resp, fixture, init_log_capture, php_lock, req};

fn post(fixture_name: &str, query: &str, content_type: Option<&str>, body: Vec<u8>) -> Request {
    let mut r: Request = req(&format!("/{fixture_name}?{query}"), fixture_name);
    r.method = "POST".into();
    r.content_type = content_type.map(|s| s.as_bytes().to_vec());
    r.content_length = body.len() as i64;
    r.body = php_sys::types::Body::Raw(std::io::Cursor::new(body));
    r
}

/// The captured `app`-target messages starting with `prefix`, in order.
fn app_messages(prefix: &str) -> Vec<String> {
    captured()
        .iter()
        .filter(|c| c.target == "app" && c.message.starts_with(prefix))
        .map(|c| c.message.clone())
        .collect()
}

/// POST form body parses into $_POST while the query string populates $_GET.
#[test]
fn post_superglobals_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
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

/// $_GET/$_POST must be rebuilt per worker request - no stale values.
#[test]
fn post_superglobals_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/post-superglobals-worker.php",
    )))?;
    let h = r.handle();
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

/// $_REQUEST merges GET + POST under the default variables_order/request_order.
#[test]
fn request_merge_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
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
fn request_merge_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/request-merge-worker.php",
    )))?;
    let h = r.handle();
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

/// A jit autoglobal first touched only in a later request must still build fresh, not stale.
#[test]
fn jit_request_superglobal_rearm_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/jit-request-worker.php")))?;
    let h = r.handle();
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

/// The Cookie header feeds $_COOKIE fresh on every worker request.
#[test]
fn cookies_refresh_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/cookies-worker.php")))?;
    let h = r.handle();
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

/// PHP's cookie parser rewrites bad name chars to underscores, drops separator-only segments, keeps trailing value spaces, and keeps the first duplicate.
#[test]
fn malformed_cookies_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
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
    let h = r.handle();

    let r1 = drain_resp(h.handle_blocking(req(&format!("/{fixture_name}"), fixture_name))?);
    assert_eq!(r1.status(), 200);
    assert_eq!(
        r1.body_string(),
        "Count: 0\n",
        "fresh session starts at zero"
    );
    let sid = r1
        .head
        .as_ref()
        .expect("head")
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
    let r2 = drain_resp(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(r2.status(), 200);
    assert_eq!(
        r2.body_string(),
        "Count: 1\n",
        "returned cookie must resume the same session"
    );
    Ok(())
}

#[test]
fn session_cookie_roundtrip_classic() -> anyhow::Result<()> {
    session_roundtrip(Mode::Classic, "ported_tests/session-count.php")
}

#[test]
fn session_cookie_roundtrip_worker() -> anyhow::Result<()> {
    session_roundtrip(
        Mode::Worker(fixture("ported_tests/session-count-worker.php")),
        "ported_tests/session-count-worker.php",
    )
}

/// A userland save handler registered during request 1 must still serve request 2.
#[test]
fn session_handler_registered_midstream_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/session-handler-worker.php",
    )))?;
    let h = r.handle();
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

/// A save handler registered before the worker loop stays installed for all requests.
#[test]
fn session_preloop_handler_preserved_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/preloop-session-handler-worker.php",
    )))?;
    let h = r.handle();
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

/// header() edges: no-space colon trims, a colon-less line never becomes a header, http_response_code sticks, and the set rebuilds per worker request.
#[test]
fn response_header_edges_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/headers-worker.php")))?;
    let h = r.handle();
    for i in [42, 43] {
        let resp = drain_resp(h.handle_blocking(req(
            &format!("/headers-worker.php?i={i}"),
            "ported_tests/headers-worker.php",
        ))?);
        assert_eq!(
            resp.status(),
            201,
            "http_response_code(201) must reach the head"
        );
        assert_eq!(resp.header("Foo").as_deref(), Some("bar"));
        assert_eq!(resp.header("Foo2").as_deref(), Some("bar2"));
        assert_eq!(
            resp.header("Foo3").as_deref(),
            Some("bar3"),
            "no-space colon must trim"
        );
        assert_eq!(resp.header("I").as_deref(), Some(format!("{i}").deref()));
        assert!(
            resp.header("Invalid").is_none(),
            "colon-less header line must not become a response header"
        );
        assert_eq!(resp.body_string(), "Hello");
    }
    drop(h);
    r.shutdown();
    Ok(())
}

fn assert_headers_list_response(resp: &Resp, i: u16) {
    assert_eq!(resp.status(), 200 + i);
    let body = resp.body_string();
    for expected in ["X-Powered-By: PHP/", "Foo: bar", "Foo2: bar2", "Invalid"] {
        assert!(
            body.contains(expected),
            "missing {expected:?} (got: {body:?})"
        );
    }
    assert!(body.contains(&format!("I: {i}")), "got: {body:?}");
    assert_eq!(resp.header("Foo").as_deref(), Some("bar"));
    assert!(
        resp.header("X-Powered-By")
            .is_some_and(|v| v.starts_with("PHP/")),
        "X-Powered-By must be a response header (headers: {:?})",
        resp.head.as_ref().expect("head").headers
    );
    assert!(resp.header("Invalid").is_none());
}

#[test]
fn headers_list_and_expose_php_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req(
        "/response-headers.php?i=1",
        "ported_tests/response-headers.php",
    ))?);
    drop(h);
    r.shutdown();
    assert_headers_list_response(&resp, 1);
    Ok(())
}

/// Worker requests must also carry the expose_php X-Powered-By header a full per-request startup adds.
#[test]
fn headers_list_and_expose_php_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/response-headers-worker.php",
    )))?;
    let h = r.handle();
    for i in [1u16, 2] {
        let resp = drain_resp(h.handle_blocking(req(
            &format!("/response-headers-worker.php?i={i}"),
            "ported_tests/response-headers-worker.php",
        ))?);
        assert_headers_list_response(&resp, i);
    }
    drop(h);
    r.shutdown();
    Ok(())
}

/// Unbuffered writes split by an explicit flush() arrive whole and in order in one sealed frame.
#[test]
fn flush_output_arrives_complete_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/flush-worker.php")))?;
    let h = r.handle();
    for i in [42, 43] {
        let rx = h.handle_blocking(req(
            &format!("/flush-worker.php?i={i}"),
            "ported_tests/flush-worker.php",
        ))?;
        let resp = drain_resp(rx);
        assert_eq!(resp.heads, 1, "exactly one head per response");
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.body_string(),
            format!("Hello {i}"),
            "flushed chunks arrive whole and in order"
        );
        assert!(!resp.truncated, "clean completion is not truncated");
    }
    drop(h);
    r.shutdown();
    Ok(())
}

/// A raw status line drives the head status, and the SAPI never suppresses the body for a 204.
#[test]
fn raw_status_line_204_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let resp =
        drain_resp(h.handle_blocking(req("/only-headers.php", "ported_tests/only-headers.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(resp.heads, 1);
    assert_eq!(resp.status(), 204);
    assert_eq!(
        resp.header("Content-Type").as_deref(),
        Some("application/json")
    );
    let headers = &resp.head.as_ref().expect("head").headers;
    assert!(
        !headers.iter().any(|(k, _)| k.starts_with("HTTP/")),
        "the raw status line must not appear as a header (headers: {headers:?})"
    );
    assert_eq!(resp.body_string(), r#"{"status": "test"}"#);
    Ok(())
}

/// A 6MB body with no content type travels intact through php://input, leaving the next request unaffected.
#[test]
fn large_post_body_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/large-request-worker.php",
    )))?;
    let h = r.handle();
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
    let h = r.handle();
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
fn multipart_upload_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/upload-worker.php")))?;
    let h = r.handle();
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

/// $_FILES has no self-healing create callback, so without a per-request dtor of TRACK_VARS_FILES a no-upload request re-exposes the previous upload.
#[test]
fn files_superglobal_does_not_leak_between_worker_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/upload-worker.php")))?;
    let h = r.handle();
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

/// An uncaught throw after output keeps the single head and echo-committed 200, appends the fatal text, and the worker survives.
#[test]
fn uncaught_exception_after_output_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/output-then-throw-worker.php")))?;
    let h = r.handle();
    for i in [1, 2] {
        let resp = drain_resp(h.handle_blocking(req(
            &format!("/output-then-throw-worker.php?i={i}"),
            "shared/output-then-throw-worker.php",
        ))?);
        assert_eq!(resp.heads, 1, "exactly one head frame (got {})", resp.heads);
        assert_eq!(resp.status(), 200, "headers were committed by the echo");
        let body = resp.body_string();
        let hello = body.find("hello");
        let uncaught = body.find(&format!("Uncaught Exception: request {i}"));
        assert!(
            hello.is_some() && uncaught.is_some() && hello < uncaught,
            "echo output must precede the fatal text (got: {body:?})"
        );
    }
    drop(h);
    r.shutdown();
    Ok(())
}

/// Per-job teardown must not destruct bootstrap-resident objects and must keep object-store handles reusable across jobs.
#[test]
fn no_destructor_sweep_between_jobs_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/preloop-destruct-worker.php",
    )))?;
    let h = r.handle();
    let (s1, b1) = drain(h.handle_blocking(req(
        "/preloop-destruct-worker.php",
        "ported_tests/preloop-destruct-worker.php",
    ))?);
    let (s2, b2) = drain(h.handle_blocking(req(
        "/preloop-destruct-worker.php",
        "ported_tests/preloop-destruct-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!((s1, s2), (200, 200));
    assert!(b1.contains("write=ok dtors=0"), "req1 (got {b1:?})");
    assert!(
        b2.contains("write=ok dtors=0"),
        "bootstrap object was destructed between jobs (got {b2:?})"
    );
    let id = |b: &str| b.split("id=").nth(1).map(str::to_owned);
    assert_eq!(
        id(&b1),
        id(&b2),
        "the per-job object's handle must be reused, not grow monotonically"
    );
    Ok(())
}

/// A destructor that throws while the job's shutdown-function table is freed must not leak a pending exception into the resident loop.
#[test]
fn throwing_destructor_after_job_stays_contained_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/dtor-throw-shutdown-worker.php",
    )))?;
    let h = r.handle();
    let (s1, b1) = drain(h.handle_blocking(req(
        "/dtor-throw-shutdown-worker.php",
        "ported_tests/dtor-throw-shutdown-worker.php",
    ))?);
    let (s2, b2) = drain(h.handle_blocking(req(
        "/dtor-throw-shutdown-worker.php",
        "ported_tests/dtor-throw-shutdown-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!((s1, b1.as_str()), (200, "served=1"));
    assert_eq!(
        (s2, b2.as_str()),
        (200, "served=2"),
        "the cycle must survive a throwing post-job destructor"
    );
    Ok(())
}

/// A boot-registered shutdown function runs once, at cycle end.
#[test]
fn boot_shutdown_function_fires_once_at_worker_exit() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/boot-shutdown-worker.php",
    )))?;
    let h = r.handle();
    for _ in 0..2 {
        let (status, body) = drain(h.handle_blocking(req(
            "/boot-shutdown-worker.php",
            "ported_tests/boot-shutdown-worker.php",
        ))?);
        assert_eq!(status, 200);
        assert_eq!(
            body, "fired=0",
            "a boot-registered shutdown function must not run at a job's end"
        );
    }

    assert_eq!(
        app_messages("boot-shutdown fired="),
        Vec::<String>::new(),
        "the boot function must not run while the worker serves jobs"
    );

    drop(h);
    r.shutdown();
    assert_eq!(
        app_messages("boot-shutdown fired="),
        vec!["boot-shutdown fired=1".to_owned()],
        "the boot-registered shutdown function must fire exactly once, at worker exit"
    );
    Ok(())
}

/// A shutdown function registered during a job runs at the end of that job, exactly once. The boot-registered one fires once, at cycle end.
#[test]
fn job_shutdown_function_fires_at_end_of_its_job() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/job-shutdown-worker.php",
    )))?;
    let h = r.handle();
    for want in [
        "req=1 job_fired=0 boot_fired=0",
        "req=2 job_fired=1 boot_fired=0",
        "req=3 job_fired=1 boot_fired=0",
    ] {
        let (status, body) = drain(h.handle_blocking(req(
            "/job-shutdown-worker.php",
            "ported_tests/job-shutdown-worker.php",
        ))?);
        assert_eq!((status, body.as_str()), (200, want));
    }

    assert_eq!(
        app_messages("job-fixture "),
        vec!["job-fixture job fired=1".to_owned()],
        "only the job-registered function runs while the worker serves jobs"
    );

    drop(h);
    r.shutdown();
    assert_eq!(
        app_messages("job-fixture "),
        vec![
            "job-fixture job fired=1".to_owned(),
            "job-fixture boot fired=1".to_owned(),
        ],
        "at worker exit the boot function fires once and the job function does not run again"
    );
    Ok(())
}

/// Boot entries run first at cycle end; a post-loop registration runs after them.
#[test]
fn late_shutdown_function_runs_after_boot_entries() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/late-shutdown-worker.php",
    )))?;
    let h = r.handle();
    for _ in 0..2 {
        let (status, body) = drain(h.handle_blocking(req(
            "/late-shutdown-worker.php",
            "ported_tests/late-shutdown-worker.php",
        ))?);
        assert_eq!((status, body.as_str()), (200, "ok"));
    }

    assert_eq!(
        app_messages("sd "),
        Vec::<String>::new(),
        "no shutdown function runs while the worker serves jobs"
    );

    drop(h);
    r.shutdown();
    assert_eq!(
        app_messages("sd "),
        vec![
            "sd boot-a".to_owned(),
            "sd boot-b".to_owned(),
            "sd late".to_owned(),
        ],
        "cycle end runs the boot entries in registration order, then the post-loop entry"
    );
    Ok(())
}

/// A fatal inside a boot shutdown function leaves the worker's exit path clean.
#[test]
fn fatal_in_boot_shutdown_function_exits_clean() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture(
        "ported_tests/shutdown-fatal-boot-worker.php",
    )))?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req(
        "/shutdown-fatal-boot-worker.php",
        "ported_tests/shutdown-fatal-boot-worker.php",
    ))?);
    assert_eq!((status, body.as_str()), (200, "ok"));

    drop(h);
    r.shutdown();
    assert!(
        captured()
            .iter()
            .any(|c| c.message.contains("boot shutdown bomb")),
        "the fatal from the boot shutdown function must reach the log"
    );
    Ok(())
}

/// A refcount-1 object in a bare boot-level global must stay in the symbol table across jobs; its __destruct runs once, at cycle end.
#[test]
fn boot_global_object_survives_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("ported_tests/boot-global-worker.php")))?;
    let h = r.handle();
    for want in [
        "kernel=ok calls=1",
        "kernel=ok calls=2",
        "kernel=ok calls=3",
    ] {
        let (status, body) = drain(h.handle_blocking(req(
            "/boot-global-worker.php",
            "ported_tests/boot-global-worker.php",
        ))?);
        assert_eq!((status, body.as_str()), (200, want));
    }
    assert_eq!(
        app_messages("boot-kernel destructed").len(),
        0,
        "no __destruct while the worker serves jobs"
    );

    drop(h);
    r.shutdown();
    assert_eq!(
        app_messages("boot-kernel destructed").len(),
        1,
        "the boot object must destruct exactly once, at worker exit"
    );
    Ok(())
}

/// A truncated response must not carry a synthesized Content-Length: its absence is what lets clients detect the cut.
#[test]
fn truncated_response_has_no_content_length_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shared/output-then-throw-worker.php")))?;
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req(
        "/output-then-throw-worker.php?i=1",
        "shared/output-then-throw-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    assert!(resp.truncated, "uncaught throw after output must truncate");
    assert_eq!(resp.content_length, None, "got: {:?}", resp.content_length);
    Ok(())
}

/// exit() ends a classic script cleanly: complete response, not an error.
#[test]
fn exit_after_output_is_complete_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req(
        "/exit-after-output-classic.php",
        "ported_tests/exit-after-output-classic.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "complete page");
    assert!(!resp.truncated, "exit() is a clean end, not a truncation");
    assert_eq!(resp.content_length, Some("complete page".len() as u64));
    Ok(())
}

/// An uncaught throw mid-stream in classic mode stays truncated, with no length.
#[test]
fn throw_after_output_truncates_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req(
        "/throw-after-output-classic.php",
        "ported_tests/throw-after-output-classic.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!(resp.status(), 200, "the echo committed the head");
    assert!(resp.truncated, "uncaught throw after output must truncate");
    assert_eq!(resp.content_length, None, "got: {:?}", resp.content_length);
    Ok(())
}

/// Streams opened before the worker loop keep identity and read position: between-request cleanup must not touch live resources.
#[test]
fn preloop_streams_survive_requests_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/file-stream-worker.php")))?;
    let h = r.handle();
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
fn error_path_keeps_status_and_cookies() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(
        "shared/error-keeps-headers-worker.php",
    )))?;
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req("/", "shared/error-keeps-headers-worker.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(resp.heads, 1);
    assert_eq!(
        resp.status(),
        404,
        "script status must survive the fatal, not force 500"
    );
    assert!(
        resp.header("set-cookie").is_some(),
        "Set-Cookie must survive"
    );
    Ok(())
}

/// A pre-joined single Cookie line passes through the fold unchanged.
#[test]
fn multi_cookie_headers_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let mut request = req("/multi-cookie.php", "ported_tests/multi-cookie.php");
    request.headers.push(("Cookie".into(), "a=1; b=2".into()));
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();
    assert_eq!((status, body.as_str()), (200, "1,2,a=1; b=2"));
    Ok(())
}

/// ReqC::build folds per-line repeats into `$_SERVER`: list fields join on their separators (Cookie on `; `, X-Forwarded-For on `, `), a singleton field keeps its first line.
#[test]
fn per_line_repeats_fold_for_superglobals_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let mut request = req("/fold-check.php", "ported_tests/fold-check.php");
    for (name, value) in [
        ("Cookie", "a=1"),
        ("X-Forwarded-For", "1.2.3.4"),
        ("Authorization", "Bearer one"),
        ("cookie", "b=2"),
        ("x-forwarded-for", "5.6.7.8"),
        ("authorization", "Bearer two"),
    ] {
        request.headers.push((name.into(), value.into()));
    }
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();
    assert_eq!(
        (status, body.as_str()),
        (200, "1,2,a=1; b=2,1.2.3.4, 5.6.7.8,Bearer one")
    );
    Ok(())
}

#[test]
fn latin1_header_value_passes_through() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req("/", "ported_tests/latin1-header.php"))?);
    drop(h);
    r.shutdown();
    let v = resp
        .head
        .as_ref()
        .expect("head")
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
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req(
        "/error-keeps-headers.php",
        "shared/error-keeps-headers.php",
    ))?);
    drop(h);
    r.shutdown();
    assert_eq!(resp.heads, 1, "exactly one head");
    assert_eq!(
        resp.status(),
        404,
        "script status must survive the fatal, not force 500"
    );
    assert!(
        resp.header("set-cookie").is_some(),
        "session Set-Cookie must reach the client (headers: {:?})",
        resp.head.as_ref().expect("head").headers
    );
    Ok(())
}

/// Content-Type must reach php-src byte for byte: a lossily decoded boundary leaves rfc1867 hunting for one the body never contains and the upload vanishes.
#[test]
fn multipart_upload_non_utf8_boundary_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/upload-worker.php")))?;
    let h = r.handle();
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

/// sapi_header_op screens only CR, LF and NUL, so dropping the headers it lets through must not cost the status, the other headers, or the body.
#[test]
fn unrepresentable_header_does_not_sink_the_response_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("ported_tests/bad-header-worker.php")))?;
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req(
        "/bad-header-worker.php",
        "ported_tests/bad-header-worker.php",
    ))?);
    drop(h);
    r.shutdown();

    assert_eq!(resp.heads, 1, "a head must still be produced");
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.body_string(), "body");
    assert_eq!(resp.header("X-Keep").as_deref(), Some("kept"));
    assert!(resp.header("Content Type").is_none());
    assert!(resp.header("X-Ctl").is_none());
    Ok(())
}
