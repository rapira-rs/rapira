//! The rapira_grpc server over the wire, with `grpc/wire-worker.php` in the role of the application: the three protocols, statuses and metadata, what PHP sees, deadlines, the plugin routes, and the drain.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use http::Method;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tests::grpc::{
    Conn, ECHO_PATH, ERROR_INFO, Fields, HI, HI_FRAME, Wire, envelope, fields, status_bytes,
    status_details, web_trailers,
};
use tests::{fixture, server_log};

use crate::harness::{
    Server, Spawn, calls, exited, listen, logged_remote, scratch_dir, signal, stop,
};

/// Logs a `call` record for each call and answers as the request metadata asks.
fn wire_worker() -> PathBuf {
    fixture("grpc/wire-worker.php")
}

/// The text of the `plugin grpc` error that a worker logs when its plugin returns an error.
fn plugin_error(srv: &Server) -> Option<String> {
    server_log::records(&srv.log_file())
        .into_iter()
        .find(|c| c.target == "rapira" && c.message.starts_with("plugin grpc"))
        .map(|c| c.message)
}

#[derive(Clone, Copy)]
enum On {
    Tcp,
    Unix,
}

/// Sources: PROTOCOL-HTTP2 (envelope, `grpc-status`, 12 for an unknown method or encoding), PROTOCOL-WEB (the 0x80 trailer frame), the Connect protocol (unary request, GET for no-side-effects methods, 404 for an unknown procedure), and proto3 JSON.
#[tokio::test]
async fn each_protocol_reaches_php_over_the_wire() {
    struct Case {
        name: &'static str,
        on: On,
        wire: Wire,
        method: Method,
        path: &'static str,
        headers: Fields,
        body: &'static [u8],
        status: u16,
        grpc_status: Option<&'static str>,
        content_type: Option<&'static str>,
        /// None: not checked.
        reply: Option<&'static [u8]>,
        /// The call reaches PHP.
        php: bool,
    }
    let grpc: Fields = &[("content-type", "application/grpc"), ("te", "trailers")];
    let cases = [
        Case {
            name: "grpc over h2c tcp",
            on: On::Tcp,
            wire: Wire::H2,
            method: Method::POST,
            path: ECHO_PATH,
            headers: grpc,
            body: HI_FRAME,
            status: 200,
            grpc_status: Some("0"),
            content_type: None,
            reply: Some(HI_FRAME),
            php: true,
        },
        Case {
            name: "grpc over h2c unix",
            on: On::Unix,
            wire: Wire::H2,
            method: Method::POST,
            path: ECHO_PATH,
            headers: grpc,
            body: HI_FRAME,
            status: 200,
            grpc_status: Some("0"),
            content_type: None,
            reply: Some(HI_FRAME),
            php: true,
        },
        Case {
            name: "grpc-web binary over http/1.1",
            on: On::Tcp,
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            headers: &[("content-type", "application/grpc-web+proto")],
            body: HI_FRAME,
            status: 200,
            grpc_status: None,
            content_type: None,
            reply: Some(b"\x00\x00\x00\x00\x04\x0a\x02hi\x80\x00\x00\x00\x10grpc-status: 0\r\n"),
            php: true,
        },
        Case {
            name: "connect proto",
            on: On::Tcp,
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            headers: &[("content-type", "application/proto")],
            body: HI,
            status: 200,
            grpc_status: None,
            content_type: Some("application/proto"),
            reply: Some(HI),
            php: true,
        },
        Case {
            name: "connect json",
            on: On::Tcp,
            wire: Wire::Http1,
            method: Method::POST,
            path: ECHO_PATH,
            headers: &[("content-type", "application/json")],
            body: br#"{"text":"hi"}"#,
            status: 200,
            grpc_status: None,
            content_type: Some("application/json"),
            reply: Some(br#"{"text":"hi"}"#),
            php: true,
        },
        Case {
            name: "connect get, idempotent",
            on: On::Tcp,
            wire: Wire::Http1,
            method: Method::GET,
            path: "/rapira.test.v1.EchoService/Get?encoding=json&message=%7B%22text%22%3A%22hi%22%7D",
            headers: &[],
            body: b"",
            status: 200,
            grpc_status: None,
            content_type: Some("application/json"),
            reply: Some(br#"{"text":"hi"}"#),
            php: true,
        },
        Case {
            name: "connect get, side effects",
            on: On::Tcp,
            wire: Wire::Http1,
            method: Method::GET,
            path: "/rapira.test.v1.EchoService/Echo?encoding=json&message=%7B%22text%22%3A%22hi%22%7D",
            headers: &[],
            body: b"",
            status: 405,
            grpc_status: None,
            content_type: None,
            reply: None,
            php: false,
        },
        Case {
            name: "streaming method",
            on: On::Tcp,
            wire: Wire::H2,
            method: Method::POST,
            path: "/rapira.test.v1.EchoService/Watch",
            headers: grpc,
            body: HI_FRAME,
            status: 200,
            grpc_status: Some("12"),
            content_type: None,
            reply: Some(b""),
            php: false,
        },
        Case {
            name: "unlisted service",
            on: On::Tcp,
            wire: Wire::H2,
            method: Method::POST,
            path: "/rapira.test.v1.OtherService/Ping",
            headers: grpc,
            body: HI_FRAME,
            status: 200,
            grpc_status: Some("12"),
            content_type: None,
            reply: Some(b""),
            php: false,
        },
        Case {
            name: "unknown method over grpc",
            on: On::Tcp,
            wire: Wire::H2,
            method: Method::POST,
            path: "/x.Y/Z",
            headers: grpc,
            body: HI_FRAME,
            status: 200,
            grpc_status: Some("12"),
            content_type: None,
            reply: Some(b""),
            php: false,
        },
        Case {
            name: "unknown method over connect",
            on: On::Tcp,
            wire: Wire::Http1,
            method: Method::POST,
            path: "/x.Y/Z",
            headers: &[("content-type", "application/proto")],
            body: HI,
            status: 404,
            grpc_status: None,
            content_type: None,
            reply: None,
            php: false,
        },
        Case {
            name: "zstd request",
            on: On::Tcp,
            wire: Wire::H2,
            method: Method::POST,
            path: ECHO_PATH,
            headers: &[
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("grpc-encoding", "zstd"),
            ],
            body: b"\x01\x00\x00\x00\x04\x28\xb5\x2f\xfd",
            status: 200,
            grpc_status: Some("12"),
            content_type: None,
            reply: Some(b""),
            php: false,
        },
    ];

    let dir = scratch_dir();
    let on_tcp = Spawn::grpc(wire_worker()).json_log().spawn();
    let on_unix = Spawn::grpc(wire_worker())
        .grpc_unix(&dir.join("grpc.sock"))
        .json_log()
        .spawn();
    let tcp = listen(&on_tcp);
    let unix = listen(&on_unix);

    for case in cases {
        let (listen, srv) = match case.on {
            On::Tcp => (&tcp, &on_tcp),
            On::Unix => (&unix, &on_unix),
        };
        let before = calls(srv).len();
        let mut conn = Conn::open(listen, case.wire).await.expect("connect");
        let got = conn
            .send(case.method, case.path, case.headers, case.body)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.status, case.status, "{}: {got:?}", case.name);
        assert_eq!(
            got.grpc_status(),
            case.grpc_status,
            "{}: {got:?}",
            case.name
        );
        if let Some(content_type) = case.content_type {
            assert_eq!(
                got.headers.get("content-type").map(|v| v.as_bytes()),
                Some(content_type.as_bytes()),
                "{}",
                case.name
            );
        }
        if let Some(reply) = case.reply {
            assert_eq!(&got.body[..], reply, "{}", case.name);
        }
        let remotes: Vec<Value> = calls(srv)[before..]
            .iter()
            .map(|call| call["remote"].clone())
            .collect();
        let want: Vec<Value> = if case.php {
            vec![json!(logged_remote(&conn.peer))]
        } else {
            Vec::new()
        };
        assert_eq!(remotes, want, "{}: the calls that reached PHP", case.name);
    }

    stop(on_tcp).await;
    stop(on_unix).await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Sources: PROTOCOL-HTTP2 and PROTOCOL-WEB (the status fields, `grpc-status-details-bin`, Response-Headers without a status field), the Connect protocol (error codes, unpadded base64 detail values, `trailer-` fields of a unary response). connectrpc adds `type.googleapis.com/` to a bare name, so gRPC keeps a type URL whole and Connect sends the bare name.
#[tokio::test]
async fn statuses_and_metadata_encode_per_protocol() {
    struct Case {
        name: &'static str,
        /// PHP fails the call with NOT_FOUND and a detail of this type URL. None: PHP echoes the message. Both carry the halves.
        fails_with: Option<&'static str>,
        wire: Wire,
        content_type: &'static str,
        body: &'static [u8],
        status: u16,
        headers: Fields,
        /// The status fields: HTTP trailers for gRPC, the 0x80 frame for gRPC-Web.
        trailers: Fields,
        /// The type URL of the detail that `grpc-status-details-bin` carries. None: no such field.
        details: Option<&'static str>,
        /// None: not checked.
        reply: Option<&'static [u8]>,
    }
    const ACME: &str = "example.com/acme.Detail";
    let failed: Fields = &[
        ("grpc-status", "5"),
        ("grpc-message", "no invoice"),
        ("x-t", "w"),
        ("x-b-bin", "AQI"),
    ];
    let succeeded: Fields = &[("grpc-status", "0"), ("x-t", "w"), ("x-b-bin", "AQI")];
    let connect_halves: Fields = &[
        ("x-h", "v"),
        ("trailer-x-t", "w"),
        ("trailer-x-b-bin", "AQI"),
    ];
    let cases = [
        Case {
            name: "grpc error",
            fails_with: Some(ERROR_INFO),
            wire: Wire::H2,
            content_type: "application/grpc",
            body: HI_FRAME,
            status: 200,
            headers: &[("x-h", "v")],
            trailers: failed,
            details: Some(ERROR_INFO),
            reply: Some(b""),
        },
        Case {
            name: "grpc-web error",
            fails_with: Some(ERROR_INFO),
            wire: Wire::Http1,
            content_type: "application/grpc-web+proto",
            body: HI_FRAME,
            status: 200,
            headers: &[("x-h", "v")],
            trailers: failed,
            details: Some(ERROR_INFO),
            reply: None,
        },
        Case {
            name: "connect error",
            fails_with: Some(ERROR_INFO),
            wire: Wire::Http1,
            content_type: "application/proto",
            body: HI,
            status: 404,
            headers: connect_halves,
            trailers: &[],
            details: None,
            reply: Some(
                br#"{"code":"not_found","message":"no invoice","details":[{"type":"google.rpc.ErrorInfo","value":"CgF4"}]}"#,
            ),
        },
        Case {
            name: "grpc keeps a custom type url whole",
            fails_with: Some(ACME),
            wire: Wire::H2,
            content_type: "application/grpc",
            body: HI_FRAME,
            status: 200,
            headers: &[("x-h", "v")],
            trailers: failed,
            details: Some(ACME),
            reply: Some(b""),
        },
        Case {
            name: "connect sends the bare type name",
            fails_with: Some(ACME),
            wire: Wire::Http1,
            content_type: "application/proto",
            body: HI,
            status: 404,
            headers: connect_halves,
            trailers: &[],
            details: None,
            reply: Some(
                br#"{"code":"not_found","message":"no invoice","details":[{"type":"acme.Detail","value":"CgF4"}]}"#,
            ),
        },
        Case {
            name: "grpc success",
            fails_with: None,
            wire: Wire::H2,
            content_type: "application/grpc",
            body: HI_FRAME,
            status: 200,
            headers: &[("x-h", "v")],
            trailers: succeeded,
            details: None,
            reply: Some(HI_FRAME),
        },
        Case {
            name: "grpc-web success",
            fails_with: None,
            wire: Wire::Http1,
            content_type: "application/grpc-web+proto",
            body: HI_FRAME,
            status: 200,
            headers: &[("x-h", "v")],
            trailers: succeeded,
            details: None,
            reply: None,
        },
        Case {
            name: "connect success",
            fails_with: None,
            wire: Wire::Http1,
            content_type: "application/proto",
            body: HI,
            status: 200,
            headers: connect_halves,
            trailers: &[],
            details: None,
            reply: Some(HI),
        },
    ];

    let srv = Spawn::grpc(wire_worker()).spawn();
    for case in cases {
        let mut conn = Conn::open(&listen(&srv), case.wire).await.expect("connect");
        let mut headers = vec![
            ("content-type", case.content_type),
            ("te", "trailers"),
            ("x-halves", "1"),
        ];
        headers.extend(case.fails_with.map(|url| ("x-fail", url)));
        let got = conn
            .send(Method::POST, ECHO_PATH, &headers, case.body)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.status, case.status, "{}: {got:?}", case.name);
        let trailers = if case.content_type.starts_with("application/grpc-web") {
            web_trailers(&got.body)
        } else {
            got.trailers.clone()
        };
        for (name, want) in fields(case.headers).iter() {
            assert_eq!(
                got.headers.get(name),
                Some(want),
                "{}: {name}: {got:?}",
                case.name
            );
        }
        for name in ["grpc-status", "grpc-message"] {
            assert!(
                !got.headers.contains_key(name),
                "{}: {name} in the head: {got:?}",
                case.name
            );
        }
        for (name, want) in fields(case.trailers).iter() {
            assert_eq!(
                trailers.get(name),
                Some(want),
                "{}: {name}: {trailers:?}",
                case.name
            );
        }
        assert_eq!(
            status_details(&trailers),
            case.details.map(status_bytes),
            "{}",
            case.name
        );
        if let Some(reply) = case.reply {
            assert_eq!(
                &got.body[..],
                reply,
                "{}: {}",
                case.name,
                String::from_utf8_lossy(&got.body)
            );
        }
    }
    stop(srv).await;
}

/// Sources: the contract gRPC README (a lost call is a sanitized INTERNAL), PROTOCOL-HTTP2 (3 is INVALID_ARGUMENT, 13 INTERNAL) and the Connect protocol (the HTTP status of each code).
#[tokio::test]
async fn php_failures_map_to_statuses() {
    /// What the PHP side does with the call.
    enum PhpDoes {
        /// Drops the call without an outcome.
        Lose,
        /// Replies with the byte ff.
        ReplyFf,
        /// The call never reaches PHP.
        Unreached,
    }
    struct Case {
        name: &'static str,
        php: PhpDoes,
        wire: Wire,
        content_type: &'static str,
        body: &'static [u8],
        status: u16,
        grpc_status: Option<&'static str>,
        /// The `code` of a Connect error body.
        connect_code: Option<&'static str>,
        /// The `grpc-message` field or the Connect `message`. None: not checked.
        message: Option<&'static str>,
        /// None: not checked.
        reply: Option<&'static [u8]>,
    }
    let cases = [
        Case {
            name: "a lost call is internal",
            php: PhpDoes::Lose,
            wire: Wire::H2,
            content_type: "application/grpc",
            body: HI_FRAME,
            status: 200,
            grpc_status: Some("13"),
            connect_code: None,
            message: Some("internal error"),
            reply: Some(b""),
        },
        Case {
            name: "an undecodable reply is internal under json",
            php: PhpDoes::ReplyFf,
            wire: Wire::Http1,
            content_type: "application/json",
            body: br#"{"text":"hi"}"#,
            status: 500,
            grpc_status: None,
            connect_code: Some("internal"),
            message: Some("internal error"),
            reply: None,
        },
        Case {
            name: "an undecodable reply passes through under proto",
            php: PhpDoes::ReplyFf,
            wire: Wire::Http1,
            content_type: "application/proto",
            body: HI,
            status: 200,
            grpc_status: None,
            connect_code: None,
            message: None,
            reply: Some(&[0xff]),
        },
        Case {
            name: "malformed json never reaches php",
            php: PhpDoes::Unreached,
            wire: Wire::Http1,
            content_type: "application/json",
            body: b"{",
            status: 400,
            grpc_status: None,
            connect_code: Some("invalid_argument"),
            message: None,
            reply: None,
        },
    ];

    let srv = Spawn::grpc(wire_worker()).json_log().spawn();
    for case in cases {
        let before = calls(&srv).len();
        let mut conn = Conn::open(&listen(&srv), case.wire).await.expect("connect");
        let mut headers = vec![("content-type", case.content_type), ("te", "trailers")];
        match case.php {
            PhpDoes::Lose => headers.push(("x-reply", "none")),
            PhpDoes::ReplyFf => headers.push(("x-reply", "ff")),
            PhpDoes::Unreached => {}
        }
        let got = conn
            .send(Method::POST, ECHO_PATH, &headers, case.body)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.status, case.status, "{}: {got:?}", case.name);
        assert_eq!(
            got.grpc_status(),
            case.grpc_status,
            "{}: {got:?}",
            case.name
        );
        if let Some(code) = case.connect_code {
            let body: Value = serde_json::from_slice(&got.body)
                .unwrap_or_else(|e| panic!("{}: {e}: {got:?}", case.name));
            assert_eq!(body["code"], code, "{}: {body}", case.name);
            if let Some(message) = case.message {
                assert_eq!(body["message"], message, "{}: {body}", case.name);
            }
        } else if let Some(message) = case.message {
            assert_eq!(got.grpc_message(), Some(message), "{}: {got:?}", case.name);
        }
        if let Some(reply) = case.reply {
            assert_eq!(&got.body[..], reply, "{}", case.name);
        }
        let reached = calls(&srv).len() - before;
        let want = usize::from(!matches!(case.php, PhpDoes::Unreached));
        assert_eq!(reached, want, "{}: the calls that reached PHP", case.name);
    }
    stop(srv).await;
}

/// Sources: the contract gRPC README (`Call\Context`), PROTOCOL-HTTP2 (`grpc-timeout`) and the Connect protocol (`connect-timeout-ms`).
#[tokio::test]
async fn request_facts_reach_php() {
    struct Case {
        name: &'static str,
        wire: Wire,
        content_type: &'static str,
        timeout: (&'static str, &'static str),
        body: &'static [u8],
        /// The `Call\Context::$protocol` value.
        protocol: &'static str,
    }
    let cases = [
        Case {
            name: "grpc",
            wire: Wire::H2,
            content_type: "application/grpc",
            timeout: ("grpc-timeout", "60S"),
            body: HI_FRAME,
            protocol: "grpc",
        },
        Case {
            name: "grpc-web",
            wire: Wire::Http1,
            content_type: "application/grpc-web+proto",
            timeout: ("grpc-timeout", "60S"),
            body: HI_FRAME,
            protocol: "grpc-web",
        },
        Case {
            name: "connect",
            wire: Wire::Http1,
            content_type: "application/proto",
            timeout: ("connect-timeout-ms", "60000"),
            body: HI,
            protocol: "connect",
        },
    ];

    let srv = Spawn::grpc(wire_worker()).json_log().spawn();
    for (seen, case) in cases.iter().enumerate() {
        let mut conn = Conn::open(&listen(&srv), case.wire).await.expect("connect");
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let headers = [
            ("content-type", case.content_type),
            ("te", "trailers"),
            ("x-a", "1"),
            case.timeout,
        ];
        let got = conn
            .send(Method::POST, ECHO_PATH, &headers, case.body)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));
        assert_eq!(got.status, 200, "{}: {got:?}", case.name);

        let calls = calls(&srv);
        assert_eq!(calls.len(), seen + 1, "{}: the call reached PHP", case.name);
        let call = &calls[seen];
        assert_eq!(
            call["method"], "rapira.test.v1.EchoService/Echo",
            "{}",
            case.name
        );
        assert_eq!(call["protocol"], case.protocol, "{}", case.name);
        assert_eq!(
            call["metadata"]["x-a"],
            json!(["1"]),
            "{}: {call}",
            case.name
        );
        let deadline = call["deadline"].as_f64();
        assert!(
            deadline.is_some_and(|d| d > wall + 50.0 && d < wall + 70.0),
            "{}: deadline {deadline:?} for a 60 s timeout at {wall}",
            case.name
        );
        assert_eq!(call["remote"], logged_remote(&conn.peer), "{}", case.name);
    }
    stop(srv).await;
}

/// Sources: the contract gRPC README (the server enforces `Call\Context::$deadline`) and PROTOCOL-HTTP2 `grpc-timeout` (`200m` is 200 ms, status 4 is DEADLINE_EXCEEDED).
#[tokio::test]
async fn a_passed_deadline_drops_the_call() {
    let srv = Spawn::grpc(fixture("grpc/unary-worker.php"))
        .json_log()
        .spawn();
    let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
    // PHP holds the call until the plugin drops it.
    let got = conn
        .grpc(ECHO_PATH, &[("grpc-timeout", "200m")], &envelope(b"cancel"))
        .await
        .unwrap();

    assert_eq!(got.grpc_status(), Some("4"), "{got:?}");
    let cancel: Value =
        serde_json::from_str(&server_log::wait_app_record(&srv.log_file(), "cancel")).unwrap();
    assert_eq!(
        cancel["cancelled"], true,
        "the call must be dropped: {cancel}"
    );
    stop(srv).await;
}

/// Sources: `grpc/health/v1/health.proto` (`HealthCheckRequest.service` and `HealthCheckResponse.status` are field 1, SERVING is 1), `grpc/reflection/v1/reflection.proto` (`list_services` is field 7), and PROTOCOL-HTTP2 (12 for an unknown method).
#[tokio::test]
async fn health_and_reflection_answer_from_the_plugin() {
    struct Case {
        name: &'static str,
        reflection: bool,
        path: &'static str,
        message: &'static [u8],
        grpc_status: &'static str,
        /// Bytes that the response body contains.
        reply: &'static [u8],
    }
    const REFLECTION: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";
    let cases = [
        Case {
            name: "health check, whole server",
            reflection: false,
            path: "/grpc.health.v1.Health/Check",
            message: b"",
            grpc_status: "0",
            reply: b"\x00\x00\x00\x00\x02\x08\x01",
        },
        Case {
            name: "health check, configured service",
            reflection: false,
            path: "/grpc.health.v1.Health/Check",
            message: b"\x0a\x1arapira.test.v1.EchoService",
            grpc_status: "0",
            reply: b"\x00\x00\x00\x00\x02\x08\x01",
        },
        Case {
            name: "reflection off",
            reflection: false,
            path: REFLECTION,
            message: b"\x3a\x00",
            grpc_status: "12",
            reply: b"",
        },
        Case {
            name: "reflection on",
            reflection: true,
            path: REFLECTION,
            message: b"\x3a\x00",
            grpc_status: "0",
            reply: b"rapira.test.v1.EchoService",
        },
    ];

    let plain = Spawn::grpc(wire_worker()).json_log().spawn();
    let reflecting = Spawn::grpc(wire_worker())
        .grpc_extra("reflection = true")
        .json_log()
        .spawn();
    for case in cases {
        let srv = if case.reflection { &reflecting } else { &plain };
        let mut conn = Conn::open(&listen(srv), Wire::H2).await.expect("connect");
        let got = conn
            .grpc(case.path, &[], &envelope(case.message))
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
    assert!(
        calls(&plain).is_empty() && calls(&reflecting).is_empty(),
        "the plugin answers without PHP"
    );
    stop(plain).await;
    stop(reflecting).await;
}

/// Stop ends the accept loop and the drain, and the worker drops every intake clone.
#[tokio::test]
async fn shutdown_joins_the_server_and_drops_every_intake_clone() {
    let srv = Spawn::grpc(wire_worker()).json_log().spawn();
    let open = tokio::net::TcpStream::connect(srv.addr)
        .await
        .expect("the server accepts before shutdown");
    drop(open);

    let srv = stop(srv).await;

    // The last intake clone goes with the worker's sink, and receive() then throws ClosedException.
    assert!(
        server_log::records(&srv.log_file())
            .iter()
            .any(|c| c.target == "app" && c.message == "closed"),
        "every intake clone is gone"
    );
}

/// The drain grace is `[supervisor] process_control_timeout_secs` less the smaller of 5 s and half of it: 4 s gives 2 s, and 1 s gives 500 ms.
#[tokio::test]
async fn drain_waits_for_calls_within_the_grace() {
    struct Case {
        name: &'static str,
        /// PHP answers the call this long after the stop. None: PHP holds the call past the grace.
        answer_after: Option<Duration>,
        process_control_timeout_secs: u64,
        grpc_status: Option<&'static str>,
        /// A text of the plugin error. None: the plugin stops clean.
        shutdown: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "a call that ends inside the grace completes",
            answer_after: Some(Duration::from_millis(100)),
            process_control_timeout_secs: 4,
            grpc_status: Some("0"),
            shutdown: None,
        },
        Case {
            name: "a call past the grace is cut",
            answer_after: None,
            process_control_timeout_secs: 1,
            grpc_status: None,
            shutdown: Some("grpc drain timed out"),
        },
    ];
    for case in cases {
        let srv = Spawn::grpc(fixture("grpc/unary-worker.php"))
            .toml(&format!(
                "[supervisor]\nprocess_control_timeout_secs = {}",
                case.process_control_timeout_secs
            ))
            .json_log()
            .spawn();
        let marker = srv.dir.join("marker");
        // `hold:` answers once the marker exists; `cancel` holds the call until the plugin drops it.
        let (message, reached) = match case.answer_after {
            Some(_) => (format!("hold:{}", marker.display()), "held"),
            None => ("cancel".to_owned(), "got"),
        };
        let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
        let client = tokio::spawn(async move {
            conn.grpc(ECHO_PATH, &[], &envelope(message.as_bytes()))
                .await
        });
        let log = srv.log_file();
        tokio::task::spawn_blocking(move || server_log::wait_app_record(&log, reached))
            .await
            .expect("the call reached PHP");

        signal(srv.pid(), libc::SIGQUIT);
        if let Some(delay) = case.answer_after {
            tokio::time::sleep(delay).await;
            std::fs::write(&marker, b"go").unwrap();
        }
        let srv = exited(srv).await;
        let got = client.await.unwrap();
        assert_eq!(
            got.as_ref().ok().and_then(|r| r.grpc_status()),
            case.grpc_status,
            "{}: {got:?}",
            case.name
        );
        let error = plugin_error(&srv);
        match case.shutdown {
            None => assert_eq!(error, None, "{}", case.name),
            Some(text) => assert!(
                error.as_ref().is_some_and(|e| e.contains(text)),
                "{}: {error:?}",
                case.name
            ),
        }
    }
}

/// Source: the `StaticChecker` docs: `shutdown` only sets NOT_SERVING, and a `Watch` stream ends when its service is removed.
#[tokio::test]
async fn drain_ends_health_watch_streams() {
    // `process_control_timeout_secs = 4` gives a drain grace of 2 s.
    let grace = Duration::from_secs(2);
    let srv = Spawn::grpc(wire_worker())
        .toml("[supervisor]\nprocess_control_timeout_secs = 4")
        .json_log()
        .spawn();
    let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
    let watch = conn
        .request(
            Method::POST,
            "/grpc.health.v1.Health/Watch",
            &[("content-type", "application/grpc"), ("te", "trailers")],
            &envelope(b""),
        )
        .await
        .unwrap();
    let mut body = watch.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&first[..], b"\x00\x00\x00\x00\x02\x08\x01", "SERVING first");

    let started = Instant::now();
    signal(srv.pid(), libc::SIGQUIT);
    let (srv, rest) = tokio::join!(exited(srv), body.collect());
    assert_eq!(plugin_error(&srv), None);
    assert!(
        started.elapsed() < grace / 2,
        "took {:?}",
        started.elapsed()
    );
    let trailers = rest.unwrap().trailers().cloned().unwrap_or_default();
    assert_eq!(
        trailers.get("grpc-status").map(|v| v.as_bytes()),
        Some(&b"0"[..]),
        "{trailers:?}"
    );
}
