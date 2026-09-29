//! The `auth` interceptor over the wire: what gRPC, gRPC-Web and Connect clients see, the routes without a token, and the boot checks of the tokens file.

use std::path::{Path, PathBuf};

use http::Method;
use serde_json::json;
use tests::fixture;
use tests::grpc::{Conn, ECHO_PATH, HI, HI_FRAME, Wire, envelope, web_trailers};

use crate::harness::{Spawn, calls, listen, scratch_dir, stop};

/// The `[grpc]` lines that enable `auth` with the tokens file `tokens`.
fn auth(tokens: &Path) -> String {
    format!(
        "interceptors = [\"auth\"]\n[grpc.auth]\ntokens_file = \"{}\"",
        tokens.display()
    )
}

/// Sources: RFC 6750 §2.1 and §3, RFC 9110 §15.5.2, PROTOCOL-HTTP2 and PROTOCOL-WEB (16 is UNAUTHENTICATED), and the Connect protocol (`unauthenticated` is HTTP 401).
#[tokio::test]
async fn auth_rejects_a_call_before_php_on_each_protocol() {
    struct Case {
        name: &'static str,
        wire: Wire,
        method: Method,
        path: &'static str,
        /// None for a Connect GET: the request has no `content-type` and no `te`.
        content_type: Option<&'static str>,
        body: &'static [u8],
        authorization: Option<&'static str>,
        status: u16,
        /// The gRPC status: HTTP trailers for gRPC, the 0x80 frame for gRPC-Web. None for Connect.
        grpc_status: Option<&'static str>,
        reaches_php: bool,
    }
    const GRPC: &str = "application/grpc";
    const WEB: &str = "application/grpc-web+proto";
    const CONNECT: &str = "application/proto";
    /// `Get` is idempotent, so it accepts a Connect GET.
    const CONNECT_GET: &str =
        "/rapira.test.v1.EchoService/Get?encoding=json&message=%7B%22text%22%3A%22hi%22%7D";
    let cases = [
        Case {
            name: "grpc, no token",
            wire: Wire::H2,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(GRPC),
            body: HI_FRAME,
            authorization: None,
            status: 200,
            grpc_status: Some("16"),
            reaches_php: false,
        },
        Case {
            name: "grpc, wrong token",
            wire: Wire::H2,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(GRPC),
            body: HI_FRAME,
            authorization: Some("Bearer gamma-token"),
            status: 200,
            grpc_status: Some("16"),
            reaches_php: false,
        },
        Case {
            name: "grpc, valid token",
            wire: Wire::H2,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(GRPC),
            body: HI_FRAME,
            authorization: Some("Bearer alpha-token"),
            status: 200,
            grpc_status: Some("0"),
            reaches_php: true,
        },
        Case {
            name: "grpc, second token, lower-case scheme",
            wire: Wire::H2,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(GRPC),
            body: HI_FRAME,
            authorization: Some("bearer  beta.token~/+=="),
            status: 200,
            grpc_status: Some("0"),
            reaches_php: true,
        },
        Case {
            name: "grpc-web, no token",
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(WEB),
            body: HI_FRAME,
            authorization: None,
            status: 200,
            grpc_status: Some("16"),
            reaches_php: false,
        },
        Case {
            name: "grpc-web, wrong token",
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(WEB),
            body: HI_FRAME,
            authorization: Some("Bearer gamma-token"),
            status: 200,
            grpc_status: Some("16"),
            reaches_php: false,
        },
        Case {
            name: "grpc-web, valid token",
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(WEB),
            body: HI_FRAME,
            authorization: Some("Bearer alpha-token"),
            status: 200,
            grpc_status: Some("0"),
            reaches_php: true,
        },
        Case {
            name: "connect, no token",
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(CONNECT),
            body: HI,
            authorization: None,
            status: 401,
            grpc_status: None,
            reaches_php: false,
        },
        Case {
            name: "connect, wrong token",
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(CONNECT),
            body: HI,
            authorization: Some("Bearer gamma-token"),
            status: 401,
            grpc_status: None,
            reaches_php: false,
        },
        Case {
            name: "connect, valid token",
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            content_type: Some(CONNECT),
            body: HI,
            authorization: Some("Bearer alpha-token"),
            status: 200,
            grpc_status: None,
            reaches_php: true,
        },
        Case {
            name: "connect get, no token",
            wire: Wire::Http1,
            method: Method::GET,
            path: CONNECT_GET,
            content_type: None,
            body: b"",
            authorization: None,
            status: 401,
            grpc_status: None,
            reaches_php: false,
        },
        Case {
            name: "connect get, valid token",
            wire: Wire::Http1,
            method: Method::GET,
            path: CONNECT_GET,
            content_type: None,
            body: b"",
            authorization: Some("Bearer alpha-token"),
            status: 200,
            grpc_status: None,
            reaches_php: true,
        },
    ];

    let srv = Spawn::grpc(fixture("grpc/wire-worker.php"))
        .grpc_extra(&auth(&fixture("grpc/tokens")))
        .json_log()
        .spawn();
    for case in cases {
        let before = calls(&srv).len();
        let mut conn = Conn::open(&listen(&srv), case.wire).await.expect("connect");
        let mut headers = Vec::new();
        if let Some(content_type) = case.content_type {
            headers.extend([("content-type", content_type), ("te", "trailers")]);
        }
        headers.extend(case.authorization.map(|a| ("authorization", a)));
        let got = conn
            .send(case.method, case.path, &headers, case.body)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.status, case.status, "{}: {got:?}", case.name);
        if let Some(want) = case.grpc_status {
            let web = (case.content_type == Some(WEB)).then(|| web_trailers(&got.body));
            let status = match &web {
                Some(trailers) => trailers.get("grpc-status").and_then(|v| v.to_str().ok()),
                None => got.grpc_status(),
            };
            assert_eq!(status, Some(want), "{}: {got:?}", case.name);
        }
        // A rejected Connect call carries the challenge and the Connect error code.
        if case.status == 401 {
            assert_eq!(
                got.headers
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok()),
                Some("Bearer"),
                "{}: {got:?}",
                case.name
            );
            let body = String::from_utf8_lossy(&got.body);
            assert!(
                body.contains(r#""code":"unauthenticated""#),
                "{}: {body}",
                case.name
            );
        }
        let calls = calls(&srv);
        assert_eq!(
            calls.len() - before,
            usize::from(case.reaches_php),
            "{}: the calls that reached PHP",
            case.name
        );
        if case.reaches_php {
            let call = calls.last().expect("a call record");
            assert_eq!(
                call["metadata"]["authorization"],
                json!([case.authorization.expect("a token")]),
                "{}: {call}",
                case.name
            );
        }
    }
    stop(srv).await;
}

/// Sources: `grpc/health/v1/health.proto` (SERVING is 1), `grpc/reflection/v1/reflection.proto` (`list_services` is field 7), PROTOCOL-HTTP2 (16 is UNAUTHENTICATED, 12 is UNIMPLEMENTED), and `crates/plugins/grpc/README.md` (health needs no token; an unknown path without a token gets 16).
#[tokio::test]
async fn auth_covers_the_plugin_routes() {
    struct Case {
        name: &'static str,
        path: &'static str,
        message: &'static [u8],
        authorization: Option<&'static str>,
        grpc_status: &'static str,
        /// Bytes that the response body contains.
        reply: &'static [u8],
    }
    const REFLECTION: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";
    const MISSING: &str = "/rapira.test.v1.EchoService/Missing";
    let cases = [
        Case {
            name: "health check without a token",
            path: "/grpc.health.v1.Health/Check",
            message: b"",
            authorization: None,
            grpc_status: "0",
            reply: b"\x00\x00\x00\x00\x02\x08\x01",
        },
        Case {
            name: "reflection without a token",
            path: REFLECTION,
            message: b"\x3a\x00",
            authorization: None,
            grpc_status: "16",
            reply: b"",
        },
        Case {
            name: "reflection with a valid token",
            path: REFLECTION,
            message: b"\x3a\x00",
            authorization: Some("Bearer alpha-token"),
            grpc_status: "0",
            reply: b"rapira.test.v1.EchoService",
        },
        Case {
            name: "unknown method without a token",
            path: MISSING,
            message: HI,
            authorization: None,
            grpc_status: "16",
            reply: b"",
        },
        Case {
            name: "unknown method with a valid token",
            path: MISSING,
            message: HI,
            authorization: Some("Bearer alpha-token"),
            grpc_status: "12",
            reply: b"",
        },
    ];

    let srv = Spawn::grpc(fixture("grpc/wire-worker.php"))
        .grpc_extra(&format!(
            "reflection = true\n{}",
            auth(&fixture("grpc/tokens"))
        ))
        .json_log()
        .spawn();
    for case in cases {
        let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
        let headers = case.authorization.map(|a| ("authorization", a));
        let got = conn
            .grpc(case.path, headers.as_slice(), &envelope(case.message))
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));
        assert_eq!(
            got.grpc_status(),
            Some(case.grpc_status),
            "{}: {got:?}",
            case.name
        );
        assert!(
            case.reply.is_empty() || got.body.windows(case.reply.len()).any(|w| w == case.reply),
            "{}: {got:?}",
            case.name
        );
    }
    assert!(calls(&srv).is_empty(), "no case reaches PHP");
    stop(srv).await;
}

/// The master reads the tokens file before the fork. `main` returns the error, so the process exits 1. The error names the line, never its text.
#[test]
fn a_bad_tokens_file_fails_the_boot() {
    struct Case {
        name: &'static str,
        tokens: PathBuf,
        log: &'static str,
    }
    let cases = [
        Case {
            name: "missing file",
            tokens: scratch_dir().join("missing-tokens"),
            log: "reading grpc.auth.tokens_file",
        },
        Case {
            name: "invalid line",
            tokens: fixture("grpc/tokens-invalid"),
            log: "line 2 is not a valid bearer token",
        },
    ];
    for case in cases {
        let (status, log) = Spawn::grpc(fixture("grpc/wire-worker.php"))
            .grpc_extra(&auth(&case.tokens))
            .boot_failure();
        assert_eq!(status.code(), Some(1), "{}: {log}", case.name);
        assert!(log.contains(case.log), "{}: {log}", case.name);
        assert!(
            !log.contains("not a token"),
            "{}: the log shows the line: {log}",
            case.name
        );
    }
}
