use std::time::{Duration, UNIX_EPOCH};

use http::{HeaderMap, Method};
use rapira_net::ListenAddr;
use rapira_sapi::Mode;
use serde_json::{Value, json};
use tests::grpc::{
    Conn, ECHO_PATH, ERROR_INFO, Fields, Response, Wire, envelope, fields, status_bytes,
    status_details,
};
use tests::{fixture, server_log};

use crate::harness::{Server, Spawn, listen, logged_remote, scratch_dir, stop};

/// Waits for the boot probe of unary-worker.php, so no call races the boot.
fn booted(srv: Server) -> Server {
    server_log::wait_app_record(&srv.log_file(), "try");
    srv
}

/// A gRPC worker on unary-worker.php, past its boot probe.
fn unary_worker() -> Server {
    booted(
        Spawn::grpc(fixture("grpc/unary-worker.php"))
            .json_log()
            .spawn(),
    )
}

/// A unary call to EchoService/Echo over h2 with `message` in the envelope, bounded at 10 s.
async fn call(listen: ListenAddr, message: Vec<u8>, headers: Fields) -> Response {
    let exchange = async {
        let mut conn = Conn::open(&listen, Wire::H2).await?;
        conn.grpc(ECHO_PATH, headers, &envelope(&message)).await
    };
    tokio::time::timeout(Duration::from_secs(10), exchange)
        .await
        .expect("no outcome within 10 s")
        .expect("the call")
}

/// Polls the log of `srv` for 10 s until it holds `n` `dispatcher` app records; returns their contexts.
fn dispatcher_records(srv: &Server, n: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let records: Vec<Value> = server_log::records(&srv.log_file())
            .into_iter()
            .filter(|c| c.target == "app" && c.message == "dispatcher")
            .map(|c| serde_json::from_str(&c.context).expect("a dispatcher record holds JSON"))
            .collect();
        if records.len() >= n {
            return records;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{} of {n} dispatcher records within 10s",
            records.len()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Expected values: GrpcDispatcher.php and echo.proto.
#[test]
fn dispatcher_identity_and_services() -> anyhow::Result<()> {
    let mut srv = Spawn::grpc(fixture("grpc/identity.php")).json_log().spawn();
    // The worker runs the script once when it starts. The stop comes after the record, so the log holds every record.
    server_log::wait_app_record(&srv.log_file(), "dispatcher");
    srv.stop();
    let ctx = server_log::dispatcher_record(&srv.log_file())?;

    let method = |name: &str, kind: &str| {
        json!({
            "class": "Rapira\\Grpc\\MethodInfo",
            "name": name,
            "inputType": "rapira.test.v1.EchoRequest",
            "outputType": "rapira.test.v1.EchoResponse",
            "kind": kind,
        })
    };
    assert_eq!(
        ctx,
        json!({
            "class": "Rapira\\Internal\\Grpc\\Dispatcher",
            "name": "grpc",
            "same": true,
            "grpc": true,
            "base": true,
            "clone": "blocked",
            "info": "Rapira\\Internal\\Grpc\\DispatcherInfo",
            "mode": "Dispatcher",
            "services": [{
                "class": "Rapira\\Grpc\\ServiceInfo",
                "name": "rapira.test.v1.EchoService",
                "methods": [
                    method("Echo", "unary"),
                    method("Get", "unary"),
                    method("Watch", "server-streaming"),
                ],
            }],
        })
    );
    Ok(())
}

/// The gRPC service list stays with its own worker: the master sets it for the `[grpc]` pool before the fork, and the worker of the `[http]` pool still gets the HTTP dispatcher.
#[test]
fn an_http_worker_after_a_grpc_worker_keeps_the_http_dispatcher() {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/worker-singleton.php"))
        .with_grpc(fixture("grpc/identity.php"))
        .json_log()
        .spawn();
    let records = dispatcher_records(&srv, 2);
    let named =
        |name: &str| -> Vec<&Value> { records.iter().filter(|r| r["name"] == name).collect() };

    let grpc = named("grpc");
    assert_eq!(grpc.len(), 1, "{records:?}");
    let http = named("http");
    assert_eq!(http.len(), 1, "{records:?}");
    assert_eq!(http[0]["class"], "Rapira\\Internal\\Http\\Dispatcher");
}

/// What a unary call over gRPC gives the client, less the transport fields.
#[derive(Debug, PartialEq)]
struct Outcome {
    grpc_status: Option<String>,
    grpc_message: Option<String>,
    headers: HeaderMap,
    trailers: HeaderMap,
    /// The message of the response envelope.
    reply: Option<Vec<u8>>,
}

impl Outcome {
    fn of(got: &Response) -> Outcome {
        Outcome {
            grpc_status: got.grpc_status().map(str::to_owned),
            grpc_message: got.grpc_message().map(str::to_owned),
            headers: application(&got.headers),
            trailers: application(&got.trailers),
            reply: (got.body.len() >= 5).then(|| got.body[5..].to_vec()),
        }
    }
}

/// The application fields of `map`. PROTOCOL-HTTP2 gives `content-type` and the `grpc-` names to the protocol, and the server adds `date`: https://www.rfc-editor.org/rfc/rfc9110#section-6.6.1
fn application(map: &HeaderMap) -> HeaderMap {
    map.iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            name != "content-type" && name != "date" && !name.starts_with("grpc-")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

#[derive(Debug)]
enum Want {
    /// The message, with no application metadata.
    Ok(&'static str),
    /// The response headers, the trailers and the message.
    Reply(Fields, Fields, &'static str),
    /// grpc-status, grpc-message and the type URL of the one detail, with no application metadata. `tests::grpc::status_bytes` gives the `google.rpc.Status` of NOT_FOUND `no invoice` with a detail that packs 0a 01 78.
    Status(&'static str, &'static str, &'static str),
    /// PHP drops the call without an outcome: the plugin answers INTERNAL `internal error`.
    Lost,
}

impl Want {
    fn outcome(&self) -> Outcome {
        let (status, message, headers, trailers, reply) = match self {
            Self::Ok(reply) => ("0", None, &[][..], &[][..], Some(reply)),
            Self::Reply(headers, trailers, reply) => ("0", None, *headers, *trailers, Some(reply)),
            Self::Status(status, message, _) => (*status, Some(*message), &[][..], &[][..], None),
            Self::Lost => ("13", Some("internal error"), &[][..], &[][..], None),
        };
        Outcome {
            grpc_status: Some(status.to_owned()),
            grpc_message: message.map(str::to_owned),
            headers: fields(headers),
            trailers: fields(trailers),
            reply: reply.map(|r| r.as_bytes().to_vec()),
        }
    }

    /// The `google.rpc.Status` that grpc-status-details-bin must carry. None: not checked.
    fn details(&self) -> Option<Vec<u8>> {
        match self {
            Self::Status(_, _, url) => Some(status_bytes(url)),
            _ => None,
        }
    }
}

struct OutcomeCase {
    name: &'static str,
    /// Calls in order, each with its expected outcome.
    steps: &'static [(&'static str, Want)],
    /// An app record the fixture must log, and a text its `result` must contain.
    log: Option<(&'static str, &'static str)>,
}

// Expected values: the contract (Work.php, Responder.php, Status.php, ResponseMetadata.php) and RFC 4648: 01 02 is `AQI`.
const OUTCOME_CASES: &[OutcomeCase] = &[
    OutcomeCase {
        name: "respond echoes the message",
        steps: &[("echo:hi", Want::Ok("hi"))],
        log: None,
    },
    OutcomeCase {
        name: "response metadata rides the outcome",
        steps: &[(
            "meta",
            Want::Reply(&[("x-h", "v"), ("x-b-bin", "AQI")], &[("x-t", "w")], "meta"),
        )],
        log: None,
    },
    OutcomeCase {
        name: "fail carries the status triple",
        steps: &[("fail", Want::Status("5", "no invoice", ERROR_INFO))],
        log: None,
    },
    OutcomeCase {
        name: "fail with a detail that is not an ErrorDetail",
        steps: &[("bad-fail", Want::Ok("recovered"))],
        log: Some(("bad-fail", "TypeError")),
    },
    OutcomeCase {
        name: "finalizing twice",
        steps: &[("twice", Want::Ok("a"))],
        log: Some((
            "twice",
            "Rapira\\Exception\\AlreadyFinalizedError: the call was already finalized",
        )),
    },
    OutcomeCase {
        name: "receive while a call is open",
        steps: &[("busy", Want::Ok("busy"))],
        log: Some((
            "busy",
            "receive() while a Rapira\\Grpc\\UnaryCall is unfinalized",
        )),
    },
    OutcomeCase {
        name: "a dropped call is lost",
        steps: &[("drop", Want::Lost)],
        log: None,
    },
    OutcomeCase {
        name: "an uncaught throwable loses the call and the worker recovers",
        steps: &[("throw", Want::Lost), ("echo:after", Want::Ok("after"))],
        log: None,
    },
    OutcomeCase {
        name: "active count while the call is open",
        steps: &[("info", Want::Ok("1"))],
        log: None,
    },
];

#[tokio::test]
async fn unary_outcomes() {
    let srv = unary_worker();

    let mut mismatches = Vec::new();
    for c in OUTCOME_CASES {
        for (message, want) in c.steps {
            let got = call(listen(&srv), message.as_bytes().to_vec(), &[]).await;
            let outcome = Outcome::of(&got);
            let details = want.details();
            if outcome != want.outcome()
                || details.is_some_and(|d| status_details(&got.trailers) != Some(d))
            {
                mismatches.push(format!(
                    "{}: {message}: expected {want:?}, got {got:?}",
                    c.name
                ));
            }
        }
    }
    // Some probes log after they answer, so the stop comes first.
    let srv = stop(srv).await;

    for c in OUTCOME_CASES {
        if let Some((record, text)) = c.log {
            let got = server_log::app_results(&srv.log_file(), record);
            if !got.iter().any(|r| r.contains(text)) {
                mismatches.push(format!(
                    "{}: no {record} record with {text:?} (got {got:?})",
                    c.name
                ));
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

#[derive(Clone, Copy)]
enum On {
    Tcp,
    Unix,
}

struct ContextCase {
    name: &'static str,
    on: On,
    wire: Wire,
    /// Every request field: the content type selects the protocol, and the rest is metadata.
    headers: Fields,
    body: &'static [u8],
    /// The seconds of the request timeout. None: the request sends none.
    timeout: Option<f64>,
    /// The response body.
    reply: &'static [u8],
    /// The logged Context without `receivedAt`, `deadline` and `remote`, as JSON.
    expected: &'static str,
}

// Expected values: Call/Context.php, PROTOCOL-WEB (the 0x80 trailer frame), and RFC 4648: `AP8` and `AP8=` both decode to 00 ff.
// The client sends `host` itself, so the case adds none.
const CONTEXT_CASES: &[ContextCase] = &[
    ContextCase {
        name: "inet peer with a deadline",
        on: On::Tcp,
        wire: Wire::Http1,
        headers: &[
            ("content-type", "application/grpc-web+proto"),
            ("user-agent", "t/1"),
            ("x-user", "a"),
            ("x-user", "b"),
            ("x-trace-bin", "AP8"),
            ("x-pad-bin", "AP8="),
            ("x-bad-bin", "!!"),
            ("grpc-timeout", "1S"),
            ("connect-protocol-version", "1"),
            ("trailer-x", "y"),
            ("te", "trailers"),
            ("connection", "close"),
        ],
        body: b"\x00\x00\x00\x00\x07context",
        timeout: Some(1.0),
        reply: b"\x00\x00\x00\x00\x07context\x80\x00\x00\x00\x10grpc-status: 0\r\n",
        expected: r#"{
            "same": true,
            "class": "Rapira\\Grpc\\Call\\Context",
            "method": "rapira.test.v1.EchoService/Echo",
            "metadata": {"user-agent": ["t/1"], "x-user": ["a", "b"], "x-trace-bin": ["00ff"], "x-pad-bin": ["00ff"]},
            "tls": null,
            "protocol": "grpc-web"
        }"#,
    },
    ContextCase {
        name: "unix peer without a deadline",
        on: On::Unix,
        wire: Wire::Http1,
        headers: &[
            ("content-type", "application/proto"),
            ("connect-protocol-version", "1"),
        ],
        body: b"context",
        timeout: None,
        reply: b"context",
        expected: r#"{
            "same": true,
            "class": "Rapira\\Grpc\\Call\\Context",
            "method": "rapira.test.v1.EchoService/Echo",
            "metadata": {},
            "tls": null,
            "protocol": "connect"
        }"#,
    },
];

#[tokio::test]
async fn call_context_reports_the_request_facts() -> anyhow::Result<()> {
    let dir = scratch_dir();
    let on_tcp = unary_worker();
    let on_unix = booted(
        Spawn::grpc(fixture("grpc/unary-worker.php"))
            .grpc_unix(&dir.join("grpc.sock"))
            .json_log()
            .spawn(),
    );

    for c in CONTEXT_CASES {
        let srv = match c.on {
            On::Tcp => &on_tcp,
            On::Unix => &on_unix,
        };
        let listen = listen(srv);
        let mut conn = Conn::open(&listen, c.wire).await?;
        let before = UNIX_EPOCH.elapsed()?.as_secs_f64();
        let got = conn
            .send(Method::POST, ECHO_PATH, c.headers, c.body)
            .await?;
        let after = UNIX_EPOCH.elapsed()?.as_secs_f64();
        assert_eq!(got.status, 200, "{}: {got:?}", c.name);
        assert_eq!(&got.body[..], c.reply, "{}: {got:?}", c.name);

        let mut ctx: Value =
            serde_json::from_str(&server_log::wait_app_record(&srv.log_file(), "context"))?;
        let fields = ctx.as_object_mut().expect("the context is an object");
        let received_at = fields.remove("receivedAt").and_then(|v| v.as_f64());
        assert!(
            received_at.is_some_and(|t| (before..=after).contains(&t)),
            "{}: receivedAt {received_at:?} outside {before}..={after}",
            c.name
        );
        let deadline = fields.remove("deadline");
        match c.timeout {
            None => assert_eq!(deadline, Some(Value::Null), "{}", c.name),
            Some(timeout) => {
                let deadline = deadline.as_ref().and_then(Value::as_f64);
                assert!(
                    deadline.is_some_and(|d| (before + timeout..=after + timeout).contains(&d)),
                    "{}: deadline {deadline:?} outside {before}..={after} plus {timeout} s",
                    c.name
                );
            }
        }
        assert_eq!(
            fields.remove("remote"),
            Some(json!(logged_remote(&conn.peer))),
            "{}",
            c.name
        );
        let expected: Value = serde_json::from_str(c.expected)?;
        assert_eq!(ctx, expected, "{}", c.name);
    }

    stop(on_tcp).await;
    stop(on_unix).await;
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

// Expected values: ResponseMetadata.php (lower-case names, reserved names, the -bin rule, raw bytes) and its "dead with the call" docblock.
const METADATA_CASES: &[(&str, &str)] = &[
    ("the accumulator is memoized", "true"),
    ("an upper-case name is normalized", "x-up"),
    ("a reserved name", "ValueError"),
    ("-bin on addHeader", "ValueError"),
    ("headers() holds raw bytes", "0102"),
    (
        "add after respond",
        "Rapira\\Exception\\AlreadyFinalizedError",
    ),
    (
        "add after the call object is gone",
        "Rapira\\Exception\\AlreadyFinalizedError",
    ),
    ("headers() after the call object is gone", "[]"),
];

#[tokio::test]
async fn response_metadata_is_call_scoped() {
    let srv = unary_worker();
    let got = call(listen(&srv), b"md-rules".to_vec(), &[]).await;
    // The probe logs after it answers, so the stop comes first.
    let srv = stop(srv).await;
    let outcome = Outcome::of(&got);
    assert_eq!(
        (outcome.grpc_status.as_deref(), outcome.reply.as_deref()),
        (Some("0"), Some(&b"md-rules"[..])),
        "{got:?}"
    );
    server_log::assert_case_records(&srv.log_file(), METADATA_CASES);
}

/// The client resets the call while PHP holds it, so the plugin closes the call: Work.php (cancelled, finalized) and Responder.php (WorkDiscardedException, "The host closed the call first").
#[tokio::test]
async fn cancel_is_visible_to_php() {
    let srv = unary_worker();
    let log = srv.log_file();

    let client = tokio::spawn(call(listen(&srv), b"cancel".to_vec(), &[]));
    let got = log.clone();
    tokio::task::spawn_blocking(move || server_log::wait_app_record(&got, "got"))
        .await
        .expect("the call reached PHP");
    // Dropping the in-flight request resets its h2 stream.
    client.abort();
    let ctx = tokio::task::spawn_blocking(move || server_log::wait_app_record(&log, "cancel"))
        .await
        .expect("the cancel record");
    stop(srv).await;

    let ctx: Value = serde_json::from_str(&ctx).unwrap();
    assert_eq!(
        ctx,
        json!({
            "cancelled": true,
            "finalized": true,
            "respond": "Rapira\\Exception\\WorkDiscardedException: the host closed the call first",
        })
    );
}

/// A call whose deadline passed while it was queued is skipped at the pull.
#[tokio::test]
async fn a_call_closed_before_the_pull_never_reaches_php() {
    let srv = unary_worker();
    let marker = srv.dir.join("marker");
    let log = srv.log_file();

    let held = tokio::spawn(call(
        listen(&srv),
        format!("hold:{}", marker.display()).into_bytes(),
        &[],
    ));
    tokio::task::spawn_blocking(move || server_log::wait_app_record(&log, "held"))
        .await
        .expect("the call reached PHP");
    // PHP holds the only worker, so this call waits in the intake until its deadline closes it.
    let gone = call(
        listen(&srv),
        b"echo:gone".to_vec(),
        &[("grpc-timeout", "200m")],
    )
    .await;
    let kept = tokio::spawn(call(listen(&srv), b"echo:kept".to_vec(), &[]));
    std::fs::write(&marker, b"go").unwrap();

    let held = Outcome::of(&held.await.unwrap());
    let kept = Outcome::of(&kept.await.unwrap());
    let srv = stop(srv).await;

    assert_eq!(gone.grpc_status(), Some("4"), "{gone:?}");
    assert_eq!(held, Want::Ok("held").outcome());
    assert_eq!(kept, Want::Ok("kept").outcome());
    assert_eq!(
        server_log::app_results(&srv.log_file(), "echo"),
        ["kept"],
        "PHP must never see the closed call"
    );
}

/// The boot probe runs on an empty intake, so tryReceive() gives null and frees a call object that holds no state.
#[test]
fn try_receive_on_an_empty_intake_returns_null() {
    let mut srv = unary_worker();
    srv.stop();
    assert_eq!(server_log::app_results(&srv.log_file(), "try"), ["NULL"]);
}
