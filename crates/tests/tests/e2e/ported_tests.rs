use std::{ops::Deref, path::Path};

use http::header::{AUTHORIZATION, COOKIE, HeaderValue, SET_COOKIE};
use rapira_sapi::{Mode, Request};
use tests::wire::submit;
use tests::{Resp, drain, drain_resp, fixture, req, server_log};

use crate::harness::Spawn;

fn post(fixture_name: &str, query: &str, content_type: Option<&str>, body: Vec<u8>) -> Request {
    let mut r: Request = req(&format!("/{fixture_name}?{query}"));
    r.method = "POST".into();
    r.content_type = content_type.map(|s| s.as_bytes().to_vec());
    r.body = rapira_sapi::types::Body::Raw(std::io::Cursor::new(body));
    r
}

/// The `app`-target messages of `log` starting with `prefix`, in order.
fn app_messages(log: &Path, prefix: &str) -> Vec<String> {
    server_log::records(log)
        .into_iter()
        .filter(|c| c.target == "app" && c.message.starts_with(prefix))
        .map(|c| c.message)
        .collect()
}

/// POST form body parses into $_POST while the query string populates $_GET.
#[test]
fn post_superglobals_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("ported_tests/post-superglobals.php")).spawn();
    let request = post(
        "ported_tests/post-superglobals.php",
        "foo=bar&baz=buz",
        Some("application/x-www-form-urlencoded"),
        b"bam=bam&some=10".to_vec(),
    );
    let (status, body) = drain(submit(srv.addr, request)?);

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
    let srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/post-superglobals-worker.php"),
    )
    .spawn();
    let (s1, b1) = drain(submit(
        srv.addr,
        post(
            "ported_tests/post-superglobals-worker.php",
            "foo=bar&iG=42",
            Some("application/x-www-form-urlencoded"),
            b"baz=bat&i=7".to_vec(),
        ),
    )?);
    let (s2, b2) = drain(submit(
        srv.addr,
        post(
            "ported_tests/post-superglobals-worker.php",
            "foo=bar&iG=43",
            Some("application/x-www-form-urlencoded"),
            b"baz=bat&i=8".to_vec(),
        ),
    )?);

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
    let srv = Spawn::http(Mode::Classic, fixture("ported_tests/request-merge.php")).spawn();
    let (status, body) = drain(submit(
        srv.addr,
        post(
            "ported_tests/request-merge.php",
            "get_key=get_value_1",
            Some("application/x-www-form-urlencoded"),
            b"post_key=post_value_1".to_vec(),
        ),
    )?);

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
    let srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/request-merge-worker.php"),
    )
    .spawn();
    for i in 1..=3 {
        let body_bytes = format!("post_key=post_value_{i}").into_bytes();
        let (status, body) = drain(submit(
            srv.addr,
            post(
                "ported_tests/request-merge-worker.php",
                &format!("get_key=get_value_{i}"),
                Some("application/x-www-form-urlencoded"),
                body_bytes,
            ),
        )?);
        assert_eq!(status, 200);
        assert!(
            body.contains(&format!("'get_key' => 'get_value_{i}'"))
                && body.contains(&format!("'post_key' => 'post_value_{i}'")),
            "req{i}: $_REQUEST must carry only this request's data (got: {body:?})"
        );
    }
    Ok(())
}

/// A jit autoglobal first touched only in a later request must still build fresh, not stale.
#[test]
fn jit_request_superglobal_rearm_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("ported_tests/jit-request-worker.php")).spawn();
    for i in 1..=4 {
        let query = if i % 2 == 1 {
            format!("use_request=1&val={i}")
        } else {
            format!("val={i}")
        };
        let (status, body) = drain(submit(
            srv.addr,
            req(&format!("/jit-request-worker.php?{query}")),
        )?);
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
    Ok(())
}

/// The Cookie header feeds $_COOKIE fresh on every worker request.
#[test]
fn cookies_refresh_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("ported_tests/cookies-worker.php")).spawn();
    for i in 0..3 {
        let mut request = req("/cookies-worker.php");
        request.headers.append(
            COOKIE,
            HeaderValue::try_from(format!("foo=bar; i={i}")).unwrap(),
        );
        let (status, body) = drain(submit(srv.addr, request)?);
        assert_eq!(status, 200);
        assert!(
            body.contains("'foo' => 'bar'") && body.contains(&format!("'i' => '{i}'")),
            "req{i}: $_COOKIE must reflect this request's header (got: {body:?})"
        );
    }
    Ok(())
}

/// PHP's cookie parser rewrites bad name chars to underscores, drops separator-only segments, keeps trailing value spaces, and keeps the first duplicate.
#[test]
fn malformed_cookies_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("ported_tests/cookies.php")).spawn();
    let mut request = req("/cookies.php");
    request.headers.append(
        COOKIE,
        HeaderValue::from_static(
            "foo =bar; ===;;==;  .dot.=val  ; PHPSESSID=1234; dup=first; dup=second",
        ),
    );
    let (status, body) = drain(submit(srv.addr, request)?);

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
    let srv = Spawn::http(mode, fixture(fixture_name)).spawn();

    let r1 = drain_resp(submit(srv.addr, req(&format!("/{fixture_name}")))?);
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
        .get_all(SET_COOKIE)
        .iter()
        .find_map(|v| {
            let s = String::from_utf8_lossy(v.as_bytes());
            s.strip_prefix("PHPSESSID=")
                .map(|rest| rest.split(';').next().unwrap_or(rest).trim().to_string())
        })
        .expect("session cookie must be issued");

    let mut request = req(&format!("/{fixture_name}"));
    request.headers.append(
        COOKIE,
        HeaderValue::try_from(format!("PHPSESSID={sid}")).unwrap(),
    );
    let r2 = drain_resp(submit(srv.addr, request)?);

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
    session_roundtrip(Mode::Worker, "ported_tests/session-count-worker.php")
}

/// A userland save handler registered during request 1 must still serve request 2.
#[test]
fn session_handler_registered_midstream_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/session-handler-worker.php"),
    )
    .spawn();
    let (s1, b1) = drain(submit(
        srv.addr,
        req("/session-handler-worker.php?action=register"),
    )?);
    let (s2, b2) = drain(submit(srv.addr, req("/session-handler-worker.php"))?);

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
    let srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/preloop-session-handler-worker.php"),
    )
    .spawn();
    let (s1, b1) = drain(submit(
        srv.addr,
        req("/preloop-session-handler-worker.php?action=check"),
    )?);
    let (s2, b2) = drain(submit(
        srv.addr,
        req("/preloop-session-handler-worker.php?action=use_session"),
    )?);
    let (s3, b3) = drain(submit(
        srv.addr,
        req("/preloop-session-handler-worker.php?action=check"),
    )?);

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
    let srv = Spawn::http(Mode::Worker, fixture("ported_tests/headers-worker.php")).spawn();
    for i in [42, 43] {
        let resp = drain_resp(submit(
            srv.addr,
            req(&format!("/headers-worker.php?i={i}")),
        )?);
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
    let srv = Spawn::http(Mode::Classic, fixture("ported_tests/response-headers.php")).spawn();
    let resp = drain_resp(submit(srv.addr, req("/response-headers.php?i=1"))?);
    assert_headers_list_response(&resp, 1);
    Ok(())
}

/// Worker requests must also carry the expose_php X-Powered-By header a full per-request startup adds.
#[test]
fn headers_list_and_expose_php_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/response-headers-worker.php"),
    )
    .spawn();
    for i in [1u16, 2] {
        let resp = drain_resp(submit(
            srv.addr,
            req(&format!("/response-headers-worker.php?i={i}")),
        )?);
        assert_headers_list_response(&resp, i);
    }
    Ok(())
}

/// Unbuffered writes split by an explicit flush() arrive whole and in order.
#[test]
fn flush_output_arrives_complete_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("ported_tests/flush-worker.php")).spawn();
    for i in [42, 43] {
        let rx = submit(srv.addr, req(&format!("/flush-worker.php?i={i}")))?;
        let resp = drain_resp(rx);
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.body_string(),
            format!("Hello {i}"),
            "flushed chunks arrive whole and in order"
        );
        assert!(!resp.truncated, "clean completion is not truncated");
    }
    Ok(())
}

/// A raw status line drives the head status.
#[test]
fn raw_status_line_204_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("ported_tests/only-headers.php")).spawn();
    let resp = drain_resp(submit(srv.addr, req("/only-headers.php"))?);

    assert_eq!(resp.status(), 204);
    assert_eq!(
        resp.header("Content-Type").as_deref(),
        Some("application/json")
    );
    Ok(())
}

/// A 6MB body with no content type travels intact through php://input, leaving the next request unaffected.
#[test]
fn large_post_body_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/large-request-worker.php"),
    )
    .spawn();
    for _ in 0..2 {
        let (status, body) = drain(submit(
            srv.addr,
            post(
                "ported_tests/large-request-worker.php",
                "",
                None,
                vec![b'f'; 6_048_576],
            ),
        )?);
        assert_eq!(status, 200);
        assert_eq!(body, "Request body size: 6048576");
    }
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
    let mut srv = Spawn::http(Mode::Classic, fixture("ported_tests/upload.php")).spawn();
    let (status, body) = drain(submit(
        srv.addr,
        post(
            "ported_tests/upload.php",
            "",
            Some("multipart/form-data; boundary=RAPIRA"),
            multipart_body(),
        ),
    )?);
    // The stop waits for the worker exit, so the request shutdown has deleted the tmp file.
    srv.stop();
    assert_upload_and_cleanup(status, &body);
    Ok(())
}

#[test]
fn multipart_upload_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("ported_tests/upload-worker.php")).spawn();
    for _ in 0..2 {
        let (status, body) = drain(submit(
            srv.addr,
            post(
                "ported_tests/upload-worker.php",
                "",
                Some("multipart/form-data; boundary=RAPIRA"),
                multipart_body(),
            ),
        )?);
        assert_upload_and_cleanup(status, &body);
    }
    Ok(())
}

/// $_FILES has no self-healing create callback, so without a per-request dtor of TRACK_VARS_FILES a no-upload request re-exposes the previous upload.
#[test]
fn files_superglobal_does_not_leak_between_worker_requests() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("ported_tests/upload-worker.php")).spawn();
    let (s1, b1) = drain(submit(
        srv.addr,
        post(
            "ported_tests/upload-worker.php",
            "",
            Some("multipart/form-data; boundary=RAPIRA"),
            multipart_body(),
        ),
    )?);
    assert_eq!(s1, 200);
    assert!(
        b1.starts_with("foo.txt|"),
        "req1 must see the upload (got {b1:?})"
    );

    let (s2, b2) = drain(submit(srv.addr, req("/upload-worker.php"))?);
    assert_eq!(s2, 200);
    assert_eq!(
        b2, "NO FILE",
        "TRACK_VARS_FILES must reset; req2 must not see req1's upload (got {b2:?})"
    );
    Ok(())
}

/// An uncaught throw after output keeps the echo-committed 200, appends the fatal text, and the worker survives.
#[test]
fn uncaught_exception_after_output_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/output-then-throw-worker.php")).spawn();
    for i in [1, 2] {
        let resp = drain_resp(submit(
            srv.addr,
            req(&format!("/output-then-throw-worker.php?i={i}")),
        )?);
        assert_eq!(resp.status(), 200, "headers were committed by the echo");
        let body = resp.body_string();
        let hello = body.find("hello");
        let uncaught = body.find(&format!("Uncaught Exception: request {i}"));
        assert!(
            hello.is_some() && uncaught.is_some() && hello < uncaught,
            "echo output must precede the fatal text (got: {body:?})"
        );
    }
    Ok(())
}

/// Per-job teardown must not destruct bootstrap-resident objects and must keep object-store handles reusable across jobs.
#[test]
fn no_destructor_sweep_between_jobs_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/preloop-destruct-worker.php"),
    )
    .spawn();
    let (s1, b1) = drain(submit(srv.addr, req("/preloop-destruct-worker.php"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/preloop-destruct-worker.php"))?);

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
    let srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/dtor-throw-shutdown-worker.php"),
    )
    .spawn();
    let (s1, b1) = drain(submit(srv.addr, req("/dtor-throw-shutdown-worker.php"))?);
    let (s2, b2) = drain(submit(srv.addr, req("/dtor-throw-shutdown-worker.php"))?);

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
    let mut srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/boot-shutdown-worker.php"),
    )
    .json_log()
    .spawn();
    for _ in 0..2 {
        let (status, body) = drain(submit(srv.addr, req("/boot-shutdown-worker.php"))?);
        assert_eq!(status, 200);
        assert_eq!(
            body, "fired=0",
            "a boot-registered shutdown function must not run at a job's end"
        );
    }

    assert_eq!(
        app_messages(&srv.log_file(), "boot-shutdown fired="),
        Vec::<String>::new(),
        "the boot function must not run while the worker serves jobs"
    );

    srv.stop();
    assert_eq!(
        app_messages(&srv.log_file(), "boot-shutdown fired="),
        vec!["boot-shutdown fired=1".to_owned()],
        "the boot-registered shutdown function must fire exactly once, at worker exit"
    );
    Ok(())
}

/// A shutdown function registered during a job runs at the end of that job, exactly once. The boot-registered one fires once, at cycle end.
#[test]
fn job_shutdown_function_fires_at_end_of_its_job() -> anyhow::Result<()> {
    let mut srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/job-shutdown-worker.php"),
    )
    .json_log()
    .spawn();
    for want in [
        "req=1 job_fired=0 boot_fired=0",
        "req=2 job_fired=1 boot_fired=0",
        "req=3 job_fired=1 boot_fired=0",
    ] {
        let (status, body) = drain(submit(srv.addr, req("/job-shutdown-worker.php"))?);
        assert_eq!((status, body.as_str()), (200, want));
    }

    assert_eq!(
        app_messages(&srv.log_file(), "job-fixture "),
        vec!["job-fixture job fired=1".to_owned()],
        "only the job-registered function runs while the worker serves jobs"
    );

    srv.stop();
    assert_eq!(
        app_messages(&srv.log_file(), "job-fixture "),
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
    let mut srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/late-shutdown-worker.php"),
    )
    .json_log()
    .spawn();
    for _ in 0..2 {
        let (status, body) = drain(submit(srv.addr, req("/late-shutdown-worker.php"))?);
        assert_eq!((status, body.as_str()), (200, "ok"));
    }

    assert_eq!(
        app_messages(&srv.log_file(), "sd "),
        Vec::<String>::new(),
        "no shutdown function runs while the worker serves jobs"
    );

    srv.stop();
    assert_eq!(
        app_messages(&srv.log_file(), "sd "),
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
    let mut srv = Spawn::http(
        Mode::Worker,
        fixture("ported_tests/shutdown-fatal-boot-worker.php"),
    )
    .json_log()
    .spawn();
    let (status, body) = drain(submit(srv.addr, req("/shutdown-fatal-boot-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "ok"));

    srv.stop();
    assert!(
        server_log::records(&srv.log_file())
            .iter()
            .any(|c| c.message.contains("boot shutdown bomb")),
        "the fatal from the boot shutdown function must reach the log"
    );
    Ok(())
}

/// A refcount-1 object in a bare boot-level global must stay in the symbol table across jobs; its __destruct runs once, at cycle end.
#[test]
fn boot_global_object_survives_requests() -> anyhow::Result<()> {
    let mut srv = Spawn::http(Mode::Worker, fixture("ported_tests/boot-global-worker.php"))
        .json_log()
        .spawn();
    for want in [
        "kernel=ok calls=1",
        "kernel=ok calls=2",
        "kernel=ok calls=3",
    ] {
        let (status, body) = drain(submit(srv.addr, req("/boot-global-worker.php"))?);
        assert_eq!((status, body.as_str()), (200, want));
    }
    assert_eq!(
        app_messages(&srv.log_file(), "boot-kernel destructed").len(),
        0,
        "no __destruct while the worker serves jobs"
    );

    srv.stop();
    assert_eq!(
        app_messages(&srv.log_file(), "boot-kernel destructed").len(),
        1,
        "the boot object must destruct exactly once, at worker exit"
    );
    Ok(())
}

/// A truncated response must not carry a synthesized Content-Length: its absence is what lets clients detect the cut.
#[test]
fn truncated_response_has_no_content_length_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/output-then-throw-worker.php")).spawn();
    let resp = drain_resp(submit(srv.addr, req("/output-then-throw-worker.php?i=1"))?);

    assert!(resp.truncated, "uncaught throw after output must truncate");
    assert_eq!(resp.content_length, None, "got: {:?}", resp.content_length);
    Ok(())
}

/// exit() ends a classic script cleanly: complete response, not an error.
#[test]
fn exit_after_output_is_complete_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Classic,
        fixture("ported_tests/exit-after-output-classic.php"),
    )
    .spawn();
    let resp = drain_resp(submit(srv.addr, req("/exit-after-output-classic.php"))?);

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "complete page");
    assert!(!resp.truncated, "exit() is a clean end, not a truncation");
    assert_eq!(resp.content_length, Some("complete page".len() as u64));
    Ok(())
}

/// An uncaught throw mid-stream in classic mode stays truncated, with no length.
#[test]
fn throw_after_output_truncates_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Classic,
        fixture("ported_tests/throw-after-output-classic.php"),
    )
    .spawn();
    let resp = drain_resp(submit(srv.addr, req("/throw-after-output-classic.php"))?);

    assert_eq!(resp.status(), 200, "the echo committed the head");
    assert!(resp.truncated, "uncaught throw after output must truncate");
    assert_eq!(resp.content_length, None, "got: {:?}", resp.content_length);
    Ok(())
}

/// Streams opened before the worker loop keep identity and read position: between-request cleanup must not touch live resources.
#[test]
fn preloop_streams_survive_requests_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("ported_tests/file-stream-worker.php")).spawn();
    for expected in ["word1", "word2", "word3"] {
        let (status, body) = drain(submit(srv.addr, req("/file-stream-worker.php"))?);
        assert_eq!(status, 200);
        assert_eq!(
            body, expected,
            "pre-loop stream must keep advancing cleanly"
        );
    }
    Ok(())
}

#[test]
fn error_path_keeps_status_and_cookies() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Worker,
        fixture("shared/error-keeps-headers-worker.php"),
    )
    .spawn();
    let resp = drain_resp(submit(srv.addr, req("/"))?);
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

/// A pre-joined single Cookie line passes through the join unchanged.
#[test]
fn multi_cookie_headers_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("ported_tests/multi-cookie.php")).spawn();
    let mut request = req("/multi-cookie.php");
    request
        .headers
        .append(COOKIE, HeaderValue::from_static("a=1; b=2"));
    let (status, body) = drain(submit(srv.addr, request)?);
    assert_eq!((status, body.as_str()), (200, "1,2,a=1; b=2"));
    Ok(())
}

/// Per-line repeats join for the superglobals: list fields join on their separators (Cookie on `; `, X-Forwarded-For on `, `), a singleton field keeps its first line.
#[test]
fn per_line_repeats_fold_for_superglobals_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("ported_tests/fold-check.php")).spawn();
    let mut request = req("/fold-check.php");
    let forwarded_for = http::HeaderName::from_static("x-forwarded-for");
    for (name, value) in [
        (COOKIE, "a=1"),
        (forwarded_for.clone(), "1.2.3.4"),
        (AUTHORIZATION, "Bearer one"),
        (COOKIE, "b=2"),
        (forwarded_for, "5.6.7.8"),
        (AUTHORIZATION, "Bearer two"),
    ] {
        request
            .headers
            .append(name, HeaderValue::from_static(value));
    }
    let (status, body) = drain(submit(srv.addr, request)?);
    assert_eq!(
        (status, body.as_str()),
        (200, "1,2,a=1; b=2,1.2.3.4, 5.6.7.8,Bearer one")
    );
    Ok(())
}

#[test]
fn latin1_header_value_passes_through() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("ported_tests/latin1-header.php")).spawn();
    let resp = drain_resp(submit(srv.addr, req("/"))?);
    let v = resp
        .head
        .as_ref()
        .expect("head")
        .headers
        .get("x-filename")
        .unwrap()
        .as_bytes();
    assert_eq!(v, b"caf\xE9.pdf", "0xE9 must not become U+FFFD");
    Ok(())
}

#[test]
fn error_path_keeps_status_and_cookies_classic() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("shared/error-keeps-headers.php")).spawn();
    let resp = drain_resp(submit(srv.addr, req("/error-keeps-headers.php"))?);
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
    let mut srv = Spawn::http(Mode::Worker, fixture("ported_tests/upload-worker.php")).spawn();
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
    let (status, body) = drain(submit(srv.addr, request)?);
    srv.stop();
    assert_upload_and_cleanup(status, &body);
    Ok(())
}

/// sapi_header_op screens only CR, LF and NUL, so dropping the headers it lets through must not cost the status, the other headers, or the body.
#[test]
fn unrepresentable_header_does_not_sink_the_response_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("ported_tests/bad-header-worker.php")).spawn();
    let resp = drain_resp(submit(srv.addr, req("/bad-header-worker.php"))?);

    assert_eq!(resp.status(), 201);
    assert_eq!(resp.body_string(), "body");
    assert_eq!(resp.header("X-Keep").as_deref(), Some("kept"));
    assert!(resp.header("Content Type").is_none());
    assert!(resp.header("X-Ctl").is_none());
    Ok(())
}
