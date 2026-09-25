//! The rapira_grpc server over the wire, with a fake PHP behind it: the three protocols, statuses and metadata, what PHP sees, deadlines, the host routes, and the drain.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use extension_api::{ListenAddr, RpcProtocol};
use http::Method;
use http_body_util::BodyExt;
use rapira_grpc::Config;
use serde_json::Value;
use tests::grpc::{
    Answer, Conn, ECHO_PATH, ERROR_INFO, FakePhp, Fields, HI, HI_FRAME, Wire, config, envelope,
    fields, scratch_dir, start, status_bytes, status_details, tcp, wait_until, web_trailers,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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

    let php = FakePhp::new(Answer::Echo);
    let dir = scratch_dir("wire");
    let mut on_tcp = start(config(tcp()), php.clone()).await;
    let unix = ListenAddr::Unix(dir.join("grpc.sock"));
    let mut on_unix = start(config(unix), php.clone()).await;

    for case in cases {
        let listen = match case.on {
            On::Tcp => &on_tcp.listen,
            On::Unix => &on_unix.listen,
        };
        let mut conn = Conn::open(listen, case.wire).await.expect("connect");
        let before = php.seen();
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
        let calls = php.calls.lock().unwrap();
        assert_eq!(calls.len() - before, usize::from(case.php), "{}", case.name);
        if case.php {
            assert_eq!(calls[before].remote, conn.peer, "{}", case.name);
        }
    }

    on_tcp.shutdown().await.unwrap();
    on_unix.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

/// Sources: PROTOCOL-HTTP2 and PROTOCOL-WEB (the status fields, `grpc-status-details-bin`, Response-Headers without a status field), the Connect protocol (error codes, unpadded base64 detail values, `trailer-` fields of a unary response). connectrpc adds `type.googleapis.com/` to a bare name, so gRPC keeps a type URL whole and Connect sends the bare name.
#[tokio::test]
async fn statuses_and_metadata_encode_per_protocol() {
    struct Case {
        name: &'static str,
        answer: Answer,
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
            answer: Answer::Fail(ERROR_INFO),
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
            answer: Answer::Fail(ERROR_INFO),
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
            answer: Answer::Fail(ERROR_INFO),
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
            answer: Answer::Fail(ACME),
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
            answer: Answer::Fail(ACME),
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
            answer: Answer::EchoWithMetadata,
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
            answer: Answer::EchoWithMetadata,
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
            answer: Answer::EchoWithMetadata,
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

    let php = FakePhp::new(Answer::Echo);
    let mut running = start(config(tcp()), php.clone()).await;
    for case in cases {
        php.answer(case.answer);
        let mut conn = Conn::open(&running.listen, case.wire)
            .await
            .expect("connect");
        let got = conn
            .send(
                Method::POST,
                ECHO_PATH,
                &[("content-type", case.content_type), ("te", "trailers")],
                case.body,
            )
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
    running.shutdown().await.unwrap();
}

/// Sources: the extension API (`Err` is a refusal before dispatch, `Ok(None)` a lost call), the contract gRPC README (a lost call is a sanitized INTERNAL), PROTOCOL-HTTP2 (3 is INVALID_ARGUMENT, 13 INTERNAL, 14 UNAVAILABLE) and the Connect protocol (the HTTP status of each code).
#[tokio::test]
async fn php_failures_map_to_statuses() {
    struct Case {
        name: &'static str,
        answer: Answer,
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
        /// The call reaches PHP.
        php: bool,
    }
    let cases = [
        Case {
            name: "a refused call is unavailable over grpc",
            answer: Answer::Refuse,
            wire: Wire::H2,
            content_type: "application/grpc",
            body: HI_FRAME,
            status: 200,
            grpc_status: Some("14"),
            connect_code: None,
            message: Some("worker pool saturated"),
            reply: Some(b""),
            php: false,
        },
        Case {
            name: "a refused call is unavailable over connect",
            answer: Answer::Refuse,
            wire: Wire::Http1,
            content_type: "application/proto",
            body: HI,
            status: 503,
            grpc_status: None,
            connect_code: Some("unavailable"),
            message: Some("worker pool saturated"),
            reply: None,
            php: false,
        },
        Case {
            name: "a lost call is internal",
            answer: Answer::Lose,
            wire: Wire::H2,
            content_type: "application/grpc",
            body: HI_FRAME,
            status: 200,
            grpc_status: Some("13"),
            connect_code: None,
            message: Some("internal error"),
            reply: Some(b""),
            php: true,
        },
        Case {
            name: "an undecodable reply is internal under json",
            answer: Answer::Reply(Bytes::from_static(&[0xff])),
            wire: Wire::Http1,
            content_type: "application/json",
            body: br#"{"text":"hi"}"#,
            status: 500,
            grpc_status: None,
            connect_code: Some("internal"),
            message: Some("internal error"),
            reply: None,
            php: true,
        },
        Case {
            name: "an undecodable reply passes through under proto",
            answer: Answer::Reply(Bytes::from_static(&[0xff])),
            wire: Wire::Http1,
            content_type: "application/proto",
            body: HI,
            status: 200,
            grpc_status: None,
            connect_code: None,
            message: None,
            reply: Some(&[0xff]),
            php: true,
        },
        Case {
            name: "malformed json never reaches php",
            answer: Answer::Echo,
            wire: Wire::Http1,
            content_type: "application/json",
            body: b"{",
            status: 400,
            grpc_status: None,
            connect_code: Some("invalid_argument"),
            message: None,
            reply: None,
            php: false,
        },
    ];

    let php = FakePhp::new(Answer::Echo);
    let mut running = start(config(tcp()), php.clone()).await;
    for case in cases {
        php.answer(case.answer);
        let mut conn = Conn::open(&running.listen, case.wire)
            .await
            .expect("connect");
        let before = php.seen();
        let got = conn
            .send(
                Method::POST,
                ECHO_PATH,
                &[("content-type", case.content_type), ("te", "trailers")],
                case.body,
            )
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
        assert_eq!(php.seen() - before, usize::from(case.php), "{}", case.name);
    }
    running.shutdown().await.unwrap();
}

/// Sources: the extension API (`UnaryCall`), PROTOCOL-HTTP2 (`grpc-timeout`) and the Connect protocol (`connect-timeout-ms`).
#[tokio::test]
async fn request_facts_reach_php() {
    struct Case {
        name: &'static str,
        wire: Wire,
        content_type: &'static str,
        timeout: (&'static str, &'static str),
        body: &'static [u8],
        protocol: RpcProtocol,
    }
    let cases = [
        Case {
            name: "grpc",
            wire: Wire::H2,
            content_type: "application/grpc",
            timeout: ("grpc-timeout", "60S"),
            body: HI_FRAME,
            protocol: RpcProtocol::Grpc,
        },
        Case {
            name: "grpc-web",
            wire: Wire::Http1,
            content_type: "application/grpc-web+proto",
            timeout: ("grpc-timeout", "60S"),
            body: HI_FRAME,
            protocol: RpcProtocol::GrpcWeb,
        },
        Case {
            name: "connect",
            wire: Wire::Http1,
            content_type: "application/proto",
            timeout: ("connect-timeout-ms", "60000"),
            body: HI,
            protocol: RpcProtocol::Connect,
        },
    ];

    let php = FakePhp::new(Answer::Echo);
    let mut running = start(config(tcp()), php.clone()).await;
    for case in cases {
        let mut conn = Conn::open(&running.listen, case.wire)
            .await
            .expect("connect");
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let got = conn
            .send(
                Method::POST,
                ECHO_PATH,
                &[
                    ("content-type", case.content_type),
                    ("te", "trailers"),
                    ("x-a", "1"),
                    case.timeout,
                ],
                case.body,
            )
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));
        assert_eq!(got.status, 200, "{}: {got:?}", case.name);

        let calls = php.calls.lock().unwrap();
        let call = calls.last().expect("the call reached PHP");
        assert_eq!(
            call.method, "rapira.test.v1.EchoService/Echo",
            "{}",
            case.name
        );
        assert_eq!(call.protocol, case.protocol, "{}", case.name);
        assert_eq!(
            call.metadata.get("x-a").map(|v| v.as_bytes()),
            Some(&b"1"[..]),
            "{}: {:?}",
            case.name,
            call.metadata
        );
        assert!(
            call.deadline
                .is_some_and(|d| d > wall + 50.0 && d < wall + 70.0),
            "{}: deadline {:?} for a 60 s timeout at {wall}",
            case.name,
            call.deadline
        );
        assert_eq!(call.remote, conn.peer, "{}", case.name);
    }
    running.shutdown().await.unwrap();
}

/// Sources: the contract gRPC README (the host enforces `Call\Context::$deadline`) and PROTOCOL-HTTP2 `grpc-timeout` (`200m` is 200 ms, status 4 is DEADLINE_EXCEEDED).
#[tokio::test]
async fn a_passed_deadline_drops_the_call() {
    let php = FakePhp::new(Answer::Never);
    let mut running = start(config(tcp()), php.clone()).await;
    let mut conn = Conn::open(&running.listen, Wire::H2)
        .await
        .expect("connect");
    let got = conn
        .grpc(ECHO_PATH, &[("grpc-timeout", "200m")], HI_FRAME)
        .await
        .unwrap();

    assert_eq!(got.grpc_status(), Some("4"), "{got:?}");
    assert!(
        php.dropped.load(Ordering::Acquire),
        "the call must be dropped"
    );
    running.shutdown().await.unwrap();
}

/// Source: RFC 9113, the client preface and an empty SETTINGS frame (https://www.rfc-editor.org/rfc/rfc9113#section-3.4). The peer then sends no more frames and no PING ACK.
#[tokio::test]
async fn a_silent_peer_loses_its_connection() {
    let config = Config {
        keepalive_interval: Duration::from_millis(200),
        keepalive_timeout: Duration::from_millis(200),
        ..config(tcp())
    };
    let mut running = start(config, FakePhp::new(Answer::Echo)).await;
    let &ListenAddr::Tcp(addr) = &running.listen else {
        unreachable!("tcp() listens on TCP")
    };
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        let mut buf = [0; 1024];
        while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
    })
    .await;
    assert!(closed.is_ok(), "the connection is open after 2 s");
    running.shutdown().await.unwrap();
}

/// Sources: `grpc/health/v1/health.proto` (`HealthCheckRequest.service` and `HealthCheckResponse.status` are field 1, SERVING is 1), `grpc/reflection/v1/reflection.proto` (`list_services` is field 7), and PROTOCOL-HTTP2 (12 for an unknown method).
#[tokio::test]
async fn health_and_reflection_answer_from_the_host() {
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

    let php = FakePhp::new(Answer::Echo);
    let mut plain = start(config(tcp()), php.clone()).await;
    let reflecting = Config {
        reflection: true,
        ..config(tcp())
    };
    let mut reflecting = start(reflecting, php.clone()).await;
    for case in cases {
        let listen = if case.reflection {
            &reflecting.listen
        } else {
            &plain.listen
        };
        let mut conn = Conn::open(listen, Wire::H2).await.expect("connect");
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
    assert_eq!(php.seen(), 0, "the host answers without PHP");
    plain.shutdown().await.unwrap();
    reflecting.shutdown().await.unwrap();
}

/// Mirrors the HTTP plugin: the host drops `run`, then `shutdown` joins the server thread, which frees every `Php` clone and closes the listener.
#[tokio::test]
async fn shutdown_joins_the_server_after_run_is_cancelled() {
    let backend = FakePhp::new(Answer::Echo);
    let mut running = start(config(tcp()), backend.clone()).await;
    let &ListenAddr::Tcp(addr) = &running.listen else {
        unreachable!("tcp() listens on TCP")
    };
    let open = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the server accepts before shutdown");
    drop(open);

    running.shutdown().await.unwrap();

    assert_eq!(Arc::strong_count(&backend), 1, "every Php clone is gone");
    let refused = tokio::net::TcpStream::connect(addr).await;
    assert!(refused.is_err(), "the listener is closed: {refused:?}");
}

#[tokio::test]
async fn drain_waits_for_calls_within_the_grace() {
    struct Case {
        name: &'static str,
        answer: Answer,
        grace: Duration,
        grpc_status: Option<&'static str>,
        /// A text of the shutdown error. None: shutdown succeeds.
        shutdown: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "a call that ends inside the grace completes",
            answer: Answer::Late(Duration::from_millis(100)),
            grace: Duration::from_secs(2),
            grpc_status: Some("0"),
            shutdown: None,
        },
        Case {
            name: "a call past the grace is cut",
            answer: Answer::Never,
            grace: Duration::from_millis(200),
            grpc_status: None,
            shutdown: Some("grpc drain timed out"),
        },
    ];
    for case in cases {
        let php = FakePhp::new(case.answer);
        let config = Config {
            drain_grace: case.grace,
            ..config(tcp())
        };
        let mut running = start(config, php.clone()).await;
        let mut conn = Conn::open(&running.listen, Wire::H2)
            .await
            .expect("connect");
        let call = tokio::spawn(async move { conn.grpc(ECHO_PATH, &[], HI_FRAME).await });
        wait_until(|| php.seen() == 1).await;

        let shutdown = running.shutdown().await;
        let got = call.await.unwrap();
        assert_eq!(
            got.as_ref().ok().and_then(|r| r.grpc_status()),
            case.grpc_status,
            "{}: {got:?}",
            case.name
        );
        match case.shutdown {
            None => assert!(shutdown.is_ok(), "{}: {shutdown:?}", case.name),
            Some(text) => assert!(
                shutdown
                    .as_ref()
                    .is_err_and(|e| e.to_string().contains(text)),
                "{}: {shutdown:?}",
                case.name
            ),
        }
    }
}

/// Source: the `StaticChecker` docs: `shutdown` only sets NOT_SERVING, and a `Watch` stream ends when its service is removed.
#[tokio::test]
async fn drain_ends_health_watch_streams() {
    let grace = Duration::from_secs(2);
    let config = Config {
        drain_grace: grace,
        ..config(tcp())
    };
    let mut running = start(config, FakePhp::new(Answer::Echo)).await;
    let mut conn = Conn::open(&running.listen, Wire::H2)
        .await
        .expect("connect");
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
    let (shutdown, rest) = tokio::join!(running.shutdown(), body.collect());
    assert!(shutdown.is_ok(), "{shutdown:?}");
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
