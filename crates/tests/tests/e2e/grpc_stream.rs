//! Server-streaming calls over the wire, with `grpc/stream-worker.php` as the app: the four wire forms, the header and trailer rules, the outcomes, a slow client, a client reset and the deadline.

use std::time::Duration;

use http::Method;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tests::grpc::{Conn, Envelope, Fields, WATCH_PATH, Wire, envelope, envelopes, web_trailers};
use tests::{fixture, poll, server_log};

use crate::harness::{Server, Spawn, exited, listen, plugin_error, signal, stop};

const DISCARDED: &str = "Rapira\\Exception\\WorkDiscardedException";

const GRPC: Fields = &[("content-type", "application/grpc"), ("te", "trailers")];

/// The text of the request selects what the app yields.
fn stream_worker() -> Server {
    Spawn::grpc(fixture("grpc/stream-worker.php"))
        .json_log()
        .spawn()
}

/// `EchoRequest { text }`, and `EchoResponse { text }` with the same bytes: field 1, length-delimited.
fn echo(text: &str) -> Vec<u8> {
    assert!(text.len() < 128, "the length must fit one varint byte");
    [&[0x0a, text.len() as u8], text.as_bytes()].concat()
}

/// Sources: PROTOCOL-HTTP2 (a response is the headers, the length-prefixed messages and the trailers with `grpc-status`), PROTOCOL-WEB (the trailers are the last body frame, with the flag 0x80), the Connect protocol (a streaming response is HTTP 200 with enveloped messages; its last envelope has the flag 0x02 and holds the EndStreamResponse JSON, which has no `error` key on success) and proto3 JSON.
#[tokio::test]
async fn server_streaming_over_each_protocol() {
    /// How the protocol ends a successful stream.
    enum End {
        /// HTTP trailers with `grpc-status: 0`.
        Trailers,
        /// A last envelope with the flag 0x80 that holds `grpc-status: 0`.
        WebFrame,
        /// A last envelope with the flag 0x02 that holds a JSON object with no `error` key.
        ConnectFrame,
    }
    struct Case {
        name: &'static str,
        wire: Wire,
        headers: Fields,
        /// The request message and each response message are JSON.
        json: bool,
        end: End,
    }
    let cases = [
        Case {
            name: "grpc",
            wire: Wire::H2,
            headers: GRPC,
            json: false,
            end: End::Trailers,
        },
        Case {
            name: "grpc-web",
            wire: Wire::Http1,
            headers: &[("content-type", "application/grpc-web+proto")],
            json: false,
            end: End::WebFrame,
        },
        Case {
            name: "connect proto",
            wire: Wire::H2,
            headers: &[("content-type", "application/connect+proto")],
            json: false,
            end: End::ConnectFrame,
        },
        Case {
            name: "connect json",
            wire: Wire::Http1,
            headers: &[("content-type", "application/connect+json")],
            json: true,
            end: End::ConnectFrame,
        },
    ];

    let srv = stream_worker();
    for case in cases {
        let request = if case.json {
            br#"{"text":"count"}"#.to_vec()
        } else {
            echo("count")
        };
        let mut conn = Conn::open(&listen(&srv), case.wire).await.expect("connect");
        let got = conn
            .send(Method::POST, WATCH_PATH, case.headers, &envelope(&request))
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.status, 200, "{}: {got:?}", case.name);
        let mut all = envelopes(&got.body);
        match case.end {
            End::Trailers => {
                assert_eq!(
                    got.field("grpc-status"),
                    Some("0"),
                    "{}: {got:?}",
                    case.name
                );
            }
            End::WebFrame => {
                let last = all.pop().expect("a trailer frame");
                assert_eq!(last.flags, 0x80, "{}: {last:?}", case.name);
                let trailers = web_trailers(&got.body);
                assert_eq!(
                    trailers.get("grpc-status").map(|v| v.as_bytes()),
                    Some(&b"0"[..]),
                    "{}: {trailers:?}",
                    case.name
                );
            }
            End::ConnectFrame => {
                let last = all.pop().expect("an end envelope");
                assert_eq!(last.flags, 0x02, "{}: {last:?}", case.name);
                let end: Value = serde_json::from_slice(&last.message)
                    .unwrap_or_else(|e| panic!("{}: {e}: {last:?}", case.name));
                assert!(
                    end.is_object() && end.get("error").is_none(),
                    "{}: {end}",
                    case.name
                );
            }
        }

        let flags: Vec<u8> = all.iter().map(|e| e.flags).collect();
        assert_eq!(flags, [0, 0, 0], "{}: {all:?}", case.name);
        for (envelope, text) in all.iter().zip(["1", "2", "3"]) {
            if case.json {
                let message: Value = serde_json::from_slice(&envelope.message)
                    .unwrap_or_else(|e| panic!("{}: {e}: {envelope:?}", case.name));
                assert_eq!(message, json!({ "text": text }), "{}", case.name);
            } else {
                assert_eq!(&envelope.message[..], echo(text), "{}", case.name);
            }
        }
    }
    stop(srv).await;
}

/// Sources: the contract (`StreamingResponder::respond`, `ResponseMetadata`, `HeadersAlreadyCommittedError`, and `AlreadyFinalizedError` from a second `respond()`), the ruling of rapira-rs/rapira#133 (the first yield sends the headers and fixes the trailers of a successful stream; `fail()` sends every trailer), the ruling that `fail()` inside the running generator finalizes the call and the next yield throws WorkDiscardedException, PROTOCOL-HTTP2 (5 is NOT_FOUND, 13 is INTERNAL, and a trailers-only response has its trailers in the head) and the plugin README (a call that PHP loses ends with INTERNAL `internal error`).
#[tokio::test]
async fn server_streaming_outcomes() {
    /// One app record of the probe.
    struct Record {
        message: &'static str,
        /// The context as JSON.
        context: &'static str,
    }
    struct Case {
        name: &'static str,
        /// The probe of `stream-worker.php`.
        text: &'static str,
        /// The text of each message, in order.
        messages: &'static [&'static str],
        grpc_status: &'static str,
        /// None when the response has no `grpc-message`.
        grpc_message: Option<&'static str>,
        /// Fields of the response head.
        headers: Fields,
        /// Fields of the trailers, or of the head of a trailers-only response.
        trailers: Fields,
        /// Names that the response does not have.
        absent: &'static [&'static str],
        record: Option<Record>,
    }
    // The fatal error stops the app instance, so its case is the last one.
    let cases = [
        Case {
            name: "metadata around the first yield",
            text: "meta",
            messages: &["1", "2"],
            grpc_status: "0",
            grpc_message: None,
            headers: &[("x-h", "v"), ("x-gen", "v")],
            trailers: &[("x-t", "w")],
            absent: &["x-late", "x-late-t"],
            record: Some(Record {
                message: "late-header",
                context: r#"{"result":"Rapira\\Grpc\\Exception\\HeadersAlreadyCommittedError"}"#,
            }),
        },
        Case {
            name: "a stream with no yield",
            text: "empty",
            messages: &[],
            grpc_status: "0",
            grpc_message: None,
            headers: &[("x-h", "v")],
            trailers: &[("x-t", "w")],
            absent: &[],
            record: None,
        },
        Case {
            name: "fail() with no stream",
            text: "fail-first",
            messages: &[],
            grpc_status: "5",
            grpc_message: Some("no invoice"),
            headers: &[("x-h", "v")],
            trailers: &[("x-t", "w")],
            absent: &[],
            record: None,
        },
        Case {
            name: "a throwable, then fail()",
            text: "throw",
            messages: &["1", "2"],
            grpc_status: "5",
            grpc_message: Some("no invoice"),
            headers: &[],
            trailers: &[("x-t", "w"), ("x-late-t", "w")],
            absent: &[],
            record: Some(Record {
                message: "thrown",
                context: r#"{"class":"RuntimeException","message":"boom","finalized":false}"#,
            }),
        },
        Case {
            name: "a value that is not a string",
            text: "bad-yield",
            messages: &["1"],
            grpc_status: "13",
            grpc_message: Some("bad yield"),
            headers: &[],
            trailers: &[],
            absent: &[],
            record: Some(Record {
                message: "bad-yield",
                context: r#"{"result":"TypeError"}"#,
            }),
        },
        Case {
            name: "the app drops the call",
            text: "drop",
            messages: &[],
            grpc_status: "13",
            grpc_message: Some("internal error"),
            headers: &[],
            trailers: &[],
            absent: &[],
            record: None,
        },
        Case {
            name: "a yield after fail() inside the generator",
            text: "fail-inside",
            messages: &["1"],
            grpc_status: "5",
            grpc_message: Some("no invoice"),
            headers: &[],
            trailers: &[],
            absent: &[],
            record: Some(Record {
                message: "after-fail",
                context: r#"{"result":"Rapira\\Exception\\WorkDiscardedException"}"#,
            }),
        },
        Case {
            name: "a second respond()",
            text: "twice",
            messages: &["1", "2", "3"],
            grpc_status: "0",
            grpc_message: None,
            headers: &[],
            trailers: &[],
            absent: &[],
            record: Some(Record {
                message: "second-respond",
                context: r#"{"result":"Rapira\\Exception\\AlreadyFinalizedError"}"#,
            }),
        },
        Case {
            name: "a fatal error after two messages",
            text: "fatal",
            messages: &["1", "2"],
            grpc_status: "13",
            grpc_message: Some("internal error"),
            headers: &[],
            trailers: &[],
            absent: &[],
            record: None,
        },
    ];

    let srv = stream_worker();
    let log = srv.log_file();
    for case in cases {
        let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
        let got = conn
            .grpc(WATCH_PATH, &[], &envelope(&echo(case.text)))
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        let want: Vec<Vec<u8>> = case.messages.iter().map(|text| echo(text)).collect();
        let messages: Vec<Vec<u8>> = envelopes(&got.body)
            .into_iter()
            .map(|e| e.message.to_vec())
            .collect();
        assert_eq!(messages, want, "{}: {got:?}", case.name);
        assert_eq!(
            got.grpc_status(),
            Some(case.grpc_status),
            "{}: {got:?}",
            case.name
        );
        assert_eq!(
            got.grpc_message(),
            case.grpc_message,
            "{}: {got:?}",
            case.name
        );
        for &(name, value) in case.headers {
            assert_eq!(
                got.headers.get(name).map(|v| v.as_bytes()),
                Some(value.as_bytes()),
                "{}: the header {name}: {got:?}",
                case.name
            );
        }
        for &(name, value) in case.trailers {
            assert_eq!(
                got.field(name),
                Some(value),
                "{}: the trailer {name}: {got:?}",
                case.name
            );
        }
        for &name in case.absent {
            assert_eq!(got.field(name), None, "{}: {got:?}", case.name);
        }
        if let Some(record) = &case.record {
            let context: Value =
                serde_json::from_str(&server_log::wait_app_record(&log, record.message))
                    .unwrap_or_else(|e| panic!("{}: {e}", case.name));
            let want: Value = serde_json::from_str(record.context).expect("valid JSON");
            assert_eq!(context, want, "{}", case.name);
        }
    }
    stop(srv).await;
}

/// The metadata probe and the failure probes over gRPC-Web and Connect. Sources: PROTOCOL-WEB (the trailers are the last body frame, with the flag 0x80), the Connect protocol (a streaming response has the HTTP status 200; its last envelope has the flag 0x02 and holds the EndStreamResponse JSON `{"error": {"code", "message"}, "metadata": {"name": ["value"]}}`; `error` is absent on success; `not_found` is the name of the code NOT_FOUND) and the ruling of rapira-rs/rapira#133 (the first yield fixes the trailers of a successful stream; `fail()` sends every trailer; `fail()` before the first yield sends the headers too).
#[tokio::test]
async fn server_streaming_outcomes_over_grpc_web_and_connect() {
    /// The `error` of a Connect end envelope.
    struct Error {
        code: &'static str,
        message: &'static str,
    }
    /// How the protocol ends the stream.
    enum End {
        /// A last envelope with the flag 0x80 that holds the trailers.
        WebFrame,
        /// A last envelope with the flag 0x02 that holds the EndStreamResponse JSON. None when the JSON has no `error` key.
        ConnectFrame(Option<Error>),
    }
    struct Case {
        name: &'static str,
        wire: Wire,
        headers: Fields,
        /// The probe of `stream-worker.php`.
        text: &'static str,
        /// The texts of the messages before the end.
        messages: &'static [&'static str],
        /// Fields of the response head.
        head: Fields,
        end: End,
        /// Fields of the gRPC-Web trailer frame, or the entries of the Connect `metadata`, each with one value.
        trailers: Fields,
        /// Names that the trailer frame or the Connect `metadata` does not have.
        absent: &'static [&'static str],
    }
    const WEB: Fields = &[("content-type", "application/grpc-web+proto")];
    const CONNECT: Fields = &[("content-type", "application/connect+proto")];
    let cases = [
        Case {
            name: "grpc-web meta",
            wire: Wire::Http1,
            headers: WEB,
            text: "meta",
            messages: &["1", "2"],
            head: &[("x-h", "v"), ("x-gen", "v")],
            end: End::WebFrame,
            trailers: &[("grpc-status", "0"), ("x-t", "w")],
            absent: &["x-late-t"],
        },
        Case {
            name: "grpc-web throw",
            wire: Wire::Http1,
            headers: WEB,
            text: "throw",
            messages: &["1", "2"],
            head: &[],
            end: End::WebFrame,
            trailers: &[
                ("grpc-status", "5"),
                ("grpc-message", "no invoice"),
                ("x-t", "w"),
                ("x-late-t", "w"),
            ],
            absent: &[],
        },
        Case {
            name: "connect proto meta",
            wire: Wire::H2,
            headers: CONNECT,
            text: "meta",
            messages: &["1", "2"],
            head: &[("x-h", "v"), ("x-gen", "v")],
            end: End::ConnectFrame(None),
            trailers: &[("x-t", "w")],
            absent: &["x-late-t"],
        },
        Case {
            name: "connect proto throw",
            wire: Wire::H2,
            headers: CONNECT,
            text: "throw",
            messages: &["1", "2"],
            head: &[],
            end: End::ConnectFrame(Some(Error {
                code: "not_found",
                message: "no invoice",
            })),
            trailers: &[("x-t", "w"), ("x-late-t", "w")],
            absent: &[],
        },
        Case {
            name: "connect proto fail() with no stream",
            wire: Wire::H2,
            headers: CONNECT,
            text: "fail-first",
            messages: &[],
            head: &[("x-h", "v")],
            end: End::ConnectFrame(Some(Error {
                code: "not_found",
                message: "no invoice",
            })),
            trailers: &[("x-t", "w")],
            absent: &[],
        },
    ];

    let srv = stream_worker();
    for case in cases {
        let mut conn = Conn::open(&listen(&srv), case.wire).await.expect("connect");
        let got = conn
            .send(
                Method::POST,
                WATCH_PATH,
                case.headers,
                &envelope(&echo(case.text)),
            )
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.status, 200, "{}: {got:?}", case.name);
        for &(name, value) in case.head {
            assert_eq!(
                got.headers.get(name).map(|v| v.as_bytes()),
                Some(value.as_bytes()),
                "{}: the header {name}: {got:?}",
                case.name
            );
        }
        let mut all = envelopes(&got.body);
        let last = all.pop().expect("an end envelope");
        match case.end {
            End::WebFrame => {
                assert_eq!(last.flags, 0x80, "{}: {last:?}", case.name);
                let trailers = web_trailers(&got.body);
                for &(name, value) in case.trailers {
                    assert_eq!(
                        trailers.get(name).map(|v| v.as_bytes()),
                        Some(value.as_bytes()),
                        "{}: the trailer {name}: {trailers:?}",
                        case.name
                    );
                }
                for &name in case.absent {
                    assert_eq!(trailers.get(name), None, "{}: {trailers:?}", case.name);
                }
            }
            End::ConnectFrame(error) => {
                assert_eq!(last.flags, 0x02, "{}: {last:?}", case.name);
                let end: Value = serde_json::from_slice(&last.message)
                    .unwrap_or_else(|e| panic!("{}: {e}: {last:?}", case.name));
                match error {
                    None => assert!(end.get("error").is_none(), "{}: {end}", case.name),
                    Some(error) => {
                        assert_eq!(end["error"]["code"], error.code, "{}: {end}", case.name);
                        assert_eq!(
                            end["error"]["message"], error.message,
                            "{}: {end}",
                            case.name
                        );
                    }
                }
                for &(name, value) in case.trailers {
                    assert_eq!(
                        end["metadata"][name],
                        json!([value]),
                        "{}: the trailer {name}: {end}",
                        case.name
                    );
                }
                for &name in case.absent {
                    assert!(end["metadata"].get(name).is_none(), "{}: {end}", case.name);
                }
            }
        }

        let want: Vec<Envelope> = case
            .messages
            .iter()
            .map(|text| Envelope {
                flags: 0,
                message: echo(text).into(),
            })
            .collect();
        assert_eq!(all, want, "{}", case.name);
    }
    stop(srv).await;
}

/// A client that starts to read late stops the generator at its yield, and each message arrives whole and in order. Sources: the plugin README (the plugin queues one message, and the yield waits when the buffers of the connection are full; a gRPC reply uses gzip only when the request did) and PROTOCOL-HTTP2 (the first envelope byte is the compressed flag).
#[tokio::test]
async fn a_slow_client_gets_a_large_stream_whole_and_uncompressed() {
    let srv = stream_worker();
    let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
    let response = conn
        .request(
            Method::POST,
            WATCH_PATH,
            &[
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("grpc-accept-encoding", "gzip"),
            ],
            &envelope(&echo("flood")),
        )
        .await
        .unwrap();
    // The 12.8 MiB of the stream do not fit the flow-control window of the client, so the server cannot send all of it before the client reads.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let collected = response.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned().unwrap_or_default();
    let all = envelopes(&collected.to_bytes());

    assert_eq!(all.len(), 200);
    for (i, envelope) in all.iter().enumerate() {
        assert_eq!(envelope.flags, 0, "message {i} must not be compressed");
        assert!(
            envelope.message.len() == 65536 && envelope.message.iter().all(|&b| b == i as u8),
            "message {i} must be 64 KiB of the byte {i}"
        );
    }
    assert_eq!(
        trailers.get("grpc-status").map(|v| v.as_bytes()),
        Some(&b"0"[..]),
        "{trailers:?}"
    );
    stop(srv).await;
}

/// Sources: the contract (`StreamingResponder::respond`: the generator gets WorkDiscardedException at its yield when the host closed the call, every further yield throws a new one, and a generator that catches it and ends returns `respond()` normally) and `Work` (a call that the host closed is finalized and cancelled).
#[tokio::test]
async fn a_client_reset_reaches_the_generator() {
    struct Case {
        name: &'static str,
        /// The probe of `stream-worker.php`.
        text: &'static str,
        /// The app records of the call, in order.
        records: &'static [&'static str],
    }
    let cases = [
        Case {
            name: "the generator ends at the exception",
            text: "slow",
            records: &["discarded", "finally", "returned"],
        },
        Case {
            name: "the generator yields two times more",
            text: "stubborn",
            records: &["discarded", "discarded", "discarded", "finally", "returned"],
        },
    ];
    for case in cases {
        let srv = stream_worker();
        let log = srv.log_file();
        let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
        let response = conn
            .request(Method::POST, WATCH_PATH, GRPC, &envelope(&echo(case.text)))
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));
        let mut body = response.into_body();
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(
            first.starts_with(&envelope(&echo("1"))),
            "{}: {first:?}",
            case.name
        );
        // The drop of the body resets the stream.
        drop(body);
        drop(conn);

        // The connection task sends the reset, so the wait must not block the runtime thread.
        let record = log.clone();
        let returned =
            tokio::task::spawn_blocking(move || server_log::wait_app_record(&record, "returned"))
                .await
                .expect("the returned record");
        let returned: Value = serde_json::from_str(&returned).unwrap();
        assert_eq!(
            returned,
            json!({ "finalized": true, "cancelled": true }),
            "{}",
            case.name
        );
        let order: Vec<String> = server_log::app_records(&log)
            .records
            .into_iter()
            .map(|record| record.message)
            .collect();
        assert_eq!(order, case.records, "{}", case.name);
        let discarded = server_log::app_results(&log, "discarded");
        assert!(
            discarded.iter().all(|class| class == DISCARDED),
            "{}: {discarded:?}",
            case.name
        );
        stop(srv).await;
    }
}

/// A reply message that is not the output message of the method ends a JSON stream with an error. Sources: the Connect protocol (the last envelope of a streaming response has the flag 0x02 and holds the EndStreamResponse JSON; its `error` key holds the error, and `internal` is the name of the code INTERNAL) and proto3 JSON.
#[tokio::test]
async fn a_message_that_does_not_transcode_ends_a_json_stream() {
    let srv = stream_worker();
    let mut conn = Conn::open(&listen(&srv), Wire::Http1)
        .await
        .expect("connect");
    let got = conn
        .send(
            Method::POST,
            WATCH_PATH,
            &[("content-type", "application/connect+json")],
            &envelope(br#"{"text":"garbage"}"#),
        )
        .await
        .unwrap();

    assert_eq!(got.status, 200, "{got:?}");
    let mut all = envelopes(&got.body);
    let last = all.pop().expect("an end envelope");
    assert_eq!(last.flags, 0x02, "{last:?}");
    let end: Value = serde_json::from_slice(&last.message).unwrap();
    assert_eq!(end["error"]["code"], "internal", "{end}");
    let messages: Vec<Value> = all
        .iter()
        .map(|envelope| serde_json::from_slice(&envelope.message).unwrap())
        .collect();
    assert_eq!(messages, [json!({ "text": "1" })]);
    stop(srv).await;
}

/// Sources: PROTOCOL-HTTP2 (`grpc-timeout: 200m` is 200 ms, status 4 is DEADLINE_EXCEEDED), the contract gRPC README (the server enforces `Call\Context::$deadline`) and `StreamingResponder::respond` (the generator gets WorkDiscardedException at its yield).
#[tokio::test]
async fn a_passed_deadline_ends_the_stream() {
    struct Case {
        name: &'static str,
        /// The probe of `stream-worker.php`.
        text: &'static str,
        /// The text of the first message. None when the response has no message.
        first: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "between two messages",
            text: "slow",
            first: Some("1"),
        },
        Case {
            name: "before the first yield",
            text: "slow-start",
            first: None,
        },
    ];

    let srv = stream_worker();
    let log = srv.log_file();
    for case in cases {
        let before = server_log::app_contexts(&log, "returned").len();
        let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
        let got = conn
            .grpc(
                WATCH_PATH,
                &[("grpc-timeout", "200m")],
                &envelope(&echo(case.text)),
            )
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.grpc_status(), Some("4"), "{}: {got:?}", case.name);
        let first = envelopes(&got.body)
            .into_iter()
            .next()
            .map(|e| e.message.to_vec());
        assert_eq!(first, case.first.map(echo), "{}: {got:?}", case.name);

        // The generator ends at its next yield. The next case needs the worker, so wait for the end of respond().
        let returned = poll(Duration::from_secs(10), || {
            server_log::app_contexts(&log, "returned")
                .get(before)
                .cloned()
        })
        .unwrap_or_else(|| panic!("{}: respond() did not return", case.name));
        let returned: Value = serde_json::from_str(&returned).unwrap();
        assert_eq!(
            returned,
            json!({ "finalized": true, "cancelled": true }),
            "{}",
            case.name
        );
        assert_eq!(
            server_log::app_results(&log, "discarded"),
            vec![DISCARDED; before + 1],
            "{}",
            case.name
        );
    }
    stop(srv).await;
}

/// A stream without a client deadline uses the pool's request limit. A PHP timeout after the head ends the call with INTERNAL.
#[tokio::test]
async fn the_pool_timeout_recycles_a_stream_without_a_client_deadline() -> anyhow::Result<()> {
    let srv = Spawn::grpc(fixture("grpc/stream-worker.php"))
        .grpc_pool("request_terminate_timeout_secs = 1")
        .json_log()
        .spawn();
    let mut conn = Conn::open(&listen(&srv), Wire::H2).await?;
    let slow = envelope(&echo("slow"));
    let response =
        tokio::time::timeout(Duration::from_secs(5), conn.grpc(WATCH_PATH, &[], &slow)).await??;
    assert_eq!(response.grpc_status(), Some("13"), "{response:?}");
    assert_eq!(envelopes(&response.body)[0].message.as_ref(), echo("1"));
    crate::harness::slot_line(&srv, "recycles=1 ");
    let next = conn
        .grpc(WATCH_PATH, &[], &envelope(&echo("count")))
        .await?;
    assert_eq!(next.grpc_status(), Some("0"), "{next:?}");
    assert_eq!(envelopes(&next.body).len(), 3);
    stop(srv).await;
    Ok(())
}

/// A wait of the generator at a yield ends at the deadline of the call, so a client that does not read cannot hold the worker. Sources: PROTOCOL-HTTP2 (`grpc-timeout: 300m` is 300 ms, status 4 is DEADLINE_EXCEEDED), `StreamingResponder::respond` (the generator gets WorkDiscardedException at its yield, and `respond()` throws it when the generator does not catch it) and `Work` (a call that the host closed is finalized and cancelled).
#[tokio::test]
async fn a_passed_deadline_ends_the_wait_for_a_client_that_does_not_read() {
    let srv = stream_worker();
    let log = srv.log_file();
    let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
    let response = conn
        .request(
            Method::POST,
            WATCH_PATH,
            &[
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("grpc-timeout", "300m"),
            ],
            &envelope(&echo("flood")),
        )
        .await
        .unwrap();
    // The client reads no message, so its flow-control window fills and the generator waits at a yield.
    let record = log.clone();
    let returned =
        tokio::task::spawn_blocking(move || server_log::wait_app_record(&record, "returned"))
            .await
            .expect("the returned record");
    let returned: Value = serde_json::from_str(&returned).unwrap();
    assert_eq!(returned, json!({ "finalized": true, "cancelled": true }));

    let collected = response.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned().unwrap_or_default();
    assert_eq!(
        trailers.get("grpc-status").map(|v| v.as_bytes()),
        Some(&b"4"[..]),
        "{trailers:?}"
    );
    let messages = envelopes(&collected.to_bytes()).len();
    assert!(messages < 200, "{messages} messages");
    stop(srv).await;
}

/// A passed deadline closes the call while the generator does not wait at a yield and the client does not read. Sources: PROTOCOL-HTTP2 (`grpc-timeout: 300m` is 300 ms, status 4 is DEADLINE_EXCEEDED), `Work` (`isCancelled()`: the deadline passed; `isFinalized()`: the host committed the call on the deadline) and `StreamingResponder::respond` (the host that closes the call arrives as WorkDiscardedException at the yield).
#[tokio::test]
async fn a_passed_deadline_closes_the_call_when_the_client_does_not_read() {
    let srv = stream_worker();
    let log = srv.log_file();
    let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
    let response = conn
        .request(
            Method::POST,
            WATCH_PATH,
            &[
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("grpc-timeout", "300m"),
            ],
            &envelope(&echo("stall")),
        )
        .await
        .unwrap();
    // The client reads no message, so the first message fills its flow-control window and the second message stops the poll of the response body.
    let record = log.clone();
    let next =
        tokio::task::spawn_blocking(move || server_log::wait_app_record(&record, "next-yield"))
            .await
            .expect("the next-yield record");
    let expired: Value =
        serde_json::from_str(&server_log::wait_app_record(&log, "expired")).unwrap();
    assert_eq!(expired, json!({ "finalized": true, "cancelled": true }));
    let next: Value = serde_json::from_str(&next).unwrap();
    assert_eq!(next, json!({ "result": DISCARDED }));

    let collected = response.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned().unwrap_or_default();
    assert_eq!(
        trailers.get("grpc-status").map(|v| v.as_bytes()),
        Some(&b"4"[..]),
        "{trailers:?}"
    );
    stop(srv).await;
}

/// Sources: the contract (`StreamingResponder::respond` throws WorkDiscardedException "without starting the generator" when the host closed the call first) and PROTOCOL-HTTP2 (`grpc-timeout: 200m` is 200 ms, status 4 is DEADLINE_EXCEEDED).
#[tokio::test]
async fn respond_on_a_closed_call_does_not_start_the_generator() {
    let srv = stream_worker();
    let log = srv.log_file();
    let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
    let got = conn
        .grpc(
            WATCH_PATH,
            &[("grpc-timeout", "200m")],
            &envelope(&echo("late")),
        )
        .await
        .unwrap();

    assert_eq!(got.grpc_status(), Some("4"), "{got:?}");
    // The app logs this record after respond() threw.
    server_log::wait_app_record(&log, "late");
    assert_eq!(server_log::app_results(&log, "late"), [DISCARDED]);
    assert_eq!(
        server_log::app_contexts(&log, "started"),
        Vec::<String>::new(),
        "the generator must not start"
    );
    stop(srv).await;
}

/// A stop timeout of 1 s gives a drain grace of 500 ms. A stream that stays open past the grace causes a `grpc drain timed out` error.
#[tokio::test]
async fn a_stream_past_the_drain_grace_fails_the_drain() {
    let srv = Spawn::grpc(fixture("grpc/stream-worker.php"))
        .toml("[server]\nstop_timeout_secs = 1")
        .json_log()
        .spawn();
    let mut conn = Conn::open(&listen(&srv), Wire::H2).await.expect("connect");
    // The head arrives after the first yield. With no deadline, the probe yields for up to 10 s, and the client holds the stream open.
    let _stream = conn
        .request(Method::POST, WATCH_PATH, GRPC, &envelope(&echo("slow")))
        .await
        .unwrap();

    signal(srv.pid(), libc::SIGQUIT);
    let srv = exited(srv).await;
    let error = plugin_error(&srv);
    assert!(
        error
            .as_ref()
            .is_some_and(|e| e.contains("grpc drain timed out")),
        "{error:?}"
    );
}
