use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use http::{HeaderMap, HeaderName, HeaderValue};
use php_sys::types::Addr;
use php_sys::{GrpcOutcome, GrpcProtocol, GrpcStatus, Mode, Rapira};
use serde_json::{Value, json};
use tests::{
    call, captured, echo_services, fixture, grpc_request, init_log_capture, outcome, php_lock,
    wait_app_record,
};

/// Runs one worker to its end and returns the context of its one `dispatcher` app record. The caller holds the PHP lock.
fn dispatcher_record(mode: Mode) -> anyhow::Result<Value> {
    init_log_capture();
    captured().clear();

    let r = Rapira::start(mode)?;
    drop(r);

    let all = captured();
    let records: Vec<&str> = all
        .iter()
        .filter(|c| c.target == "app" && c.message == "dispatcher")
        .map(|c| c.context.as_str())
        .collect();
    let php: Vec<&str> = all
        .iter()
        .filter(|c| c.target == "php")
        .map(|c| c.message.as_str())
        .collect();
    assert_eq!(
        records.len(),
        1,
        "one dispatcher record (got {records:?}, php: {php:?})"
    );
    Ok(serde_json::from_str(records[0])?)
}

fn grpc_mode(script: &str) -> Mode {
    Mode::GrpcDispatcher {
        script: fixture(script),
        services: echo_services(),
    }
}

/// Boots a gRPC worker on unary-worker.php and waits for its boot probe, so no call races the boot. The caller holds the PHP lock.
fn unary_worker() -> anyhow::Result<Rapira> {
    init_log_capture();
    captured().clear();
    let r = Rapira::start(grpc_mode("grpc/unary-worker.php"))?;
    wait_app_record("try");
    Ok(r)
}

/// The `result` field of each app record named `message`, in log order.
fn results(message: &str) -> Vec<String> {
    captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == message)
        .filter_map(|c| serde_json::from_str::<Value>(&c.context).ok())
        .map(|ctx| match &ctx["result"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect()
}

fn fields(lines: &[(&'static str, &'static str)]) -> HeaderMap {
    lines
        .iter()
        .map(|&(k, v)| (HeaderName::from_static(k), HeaderValue::from_static(v)))
        .collect()
}

/// Expected values come from the Rapira\Grpc\GrpcDispatcher contract and from echo.proto.
#[test]
fn dispatcher_identity_and_services() -> anyhow::Result<()> {
    let _guard = php_lock();
    let ctx = dispatcher_record(grpc_mode("grpc/identity.php"))?;

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

/// The gRPC service list stays with its own worker: a later HTTP worker gets the HTTP dispatcher.
#[test]
fn an_http_worker_after_a_grpc_worker_keeps_the_http_dispatcher() -> anyhow::Result<()> {
    let _guard = php_lock();
    let grpc = dispatcher_record(grpc_mode("grpc/identity.php"))?;
    assert_eq!(grpc["name"], "grpc");

    let http = dispatcher_record(Mode::Dispatcher(fixture("dispatcher/worker-singleton.php")))?;
    assert_eq!(http["class"], "Rapira\\Internal\\Http\\Dispatcher");
    assert_eq!(http["name"], "http");
    Ok(())
}

#[derive(Debug)]
enum Want {
    Ok(&'static str),
    Status(u32, &'static str, &'static [(&'static str, &'static [u8])]),
    Lost,
}

impl Want {
    /// The outcome with empty metadata halves, or None for a lost call.
    fn outcome(&self) -> Option<GrpcOutcome> {
        let result = match self {
            Self::Ok(message) => Ok(message.as_bytes().to_vec().into()),
            Self::Status(code, message, details) => Err(GrpcStatus {
                code: *code,
                message: (*message).to_owned(),
                details: details
                    .iter()
                    .map(|&(url, value)| (url.to_owned(), value.to_vec().into()))
                    .collect(),
            }),
            Self::Lost => return None,
        };
        Some(GrpcOutcome {
            headers: HeaderMap::new(),
            trailers: HeaderMap::new(),
            result,
        })
    }
}

struct OutcomeCase {
    name: &'static str,
    /// Calls in order, each with its expected outcome.
    steps: &'static [(&'static str, Want)],
    /// An app record the fixture must log, and a text its `result` must contain.
    log: Option<(&'static str, &'static str)>,
}

// Expected values: Work.php (finalize once, loss), Responder.php and Status.php (the status triple, the exception messages), the gRPC README (an uncaught throwable is INTERNAL, the worker recycles), and the single-flight receive().
const OUTCOME_CASES: &[OutcomeCase] = &[
    OutcomeCase {
        name: "respond echoes the message",
        steps: &[("echo:hi", Want::Ok("hi"))],
        log: None,
    },
    OutcomeCase {
        name: "fail carries the status triple",
        steps: &[(
            "fail",
            Want::Status(
                5,
                "no invoice",
                &[("type.googleapis.com/google.rpc.ErrorInfo", b"\x0a\x01x")],
            ),
        )],
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

#[test]
fn unary_outcomes() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = unary_worker()?;
    let h = r.handle();

    let mut mismatches = Vec::new();
    for c in OUTCOME_CASES {
        for (message, want) in c.steps {
            let got = outcome(call(&h, grpc_request(message))?);
            if got != want.outcome() {
                mismatches.push(format!(
                    "{}: {message}: expected {want:?}, got {got:?}",
                    c.name
                ));
            }
        }
    }
    drop(h);
    drop(r);

    for c in OUTCOME_CASES {
        if let Some((record, text)) = c.log {
            let got = results(record);
            if !got.iter().any(|r| r.contains(text)) {
                mismatches.push(format!(
                    "{}: no {record} record with {text:?} (got {got:?})",
                    c.name
                ));
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
    Ok(())
}

struct ContextCase {
    name: &'static str,
    metadata: &'static [(&'static str, &'static str)],
    deadline: Option<f64>,
    protocol: GrpcProtocol,
    remote: Addr,
    /// The logged Context without `receivedAt`, as JSON.
    expected: &'static str,
}

// Expected values: Call/Context.php (application keys only, -bin values as raw bytes, a null tls), and PROTOCOL-HTTP2 Requests with RFC 4648: `AP8` and `AP8=` both decode to 00 ff.
const CONTEXT_CASES: &[ContextCase] = &[
    ContextCase {
        name: "inet peer with a deadline",
        metadata: &[
            ("user-agent", "t/1"),
            ("x-user", "a"),
            ("x-user", "b"),
            ("x-trace-bin", "AP8"),
            ("x-pad-bin", "AP8="),
            ("x-bad-bin", "!!"),
            ("grpc-timeout", "1S"),
            ("content-type", "application/grpc"),
            ("connect-protocol-version", "1"),
            ("trailer-x", "y"),
            ("te", "trailers"),
            ("connection", "close"),
            ("host", "e"),
        ],
        deadline: Some(1_722_700_000.5),
        protocol: GrpcProtocol::GrpcWeb,
        remote: Addr::Inet(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(203, 0, 113, 7),
            44123,
        ))),
        expected: r#"{
            "same": true,
            "class": "Rapira\\Grpc\\Call\\Context",
            "method": "rapira.test.v1.EchoService/Echo",
            "metadata": {"user-agent": ["t/1"], "x-user": ["a", "b"], "x-trace-bin": ["00ff"], "x-pad-bin": ["00ff"]},
            "deadline": 1722700000.5,
            "remote": "203.0.113.7:44123",
            "tls": null,
            "protocol": "grpc-web"
        }"#,
    },
    ContextCase {
        name: "unix peer without a deadline",
        metadata: &[],
        deadline: None,
        protocol: GrpcProtocol::Connect,
        remote: Addr::Unix(None),
        expected: r#"{
            "same": true,
            "class": "Rapira\\Grpc\\Call\\Context",
            "method": "rapira.test.v1.EchoService/Echo",
            "metadata": {},
            "deadline": null,
            "remote": "unix:NULL",
            "tls": null,
            "protocol": "connect"
        }"#,
    },
];

#[test]
fn call_context_reports_the_request_facts() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = unary_worker()?;
    let h = r.handle();

    for c in CONTEXT_CASES {
        captured().clear();
        let mut req = grpc_request("context");
        req.metadata = fields(c.metadata);
        req.deadline = c.deadline;
        req.protocol = c.protocol;
        req.remote = c.remote.clone();

        let before = std::time::UNIX_EPOCH.elapsed()?.as_secs_f64();
        let rx = call(&h, req)?;
        let after = std::time::UNIX_EPOCH.elapsed()?.as_secs_f64();
        let got = outcome(rx);
        assert_eq!(
            got.map(|o| o.result),
            Some(Ok("context".into())),
            "{}",
            c.name
        );

        let mut ctx: Value = serde_json::from_str(&wait_app_record("context"))?;
        let received_at = ctx
            .as_object_mut()
            .and_then(|o| o.remove("receivedAt"))
            .and_then(|v| v.as_f64());
        assert!(
            received_at.is_some_and(|t| (before..=after).contains(&t)),
            "{}: receivedAt {received_at:?} outside {before}..={after}",
            c.name
        );
        let expected: Value = serde_json::from_str(c.expected)?;
        assert_eq!(ctx, expected, "{}", c.name);
    }

    drop(h);
    drop(r);
    Ok(())
}

/// Expected values: ResponseMetadata.php (binary values are raw, emitted as unpadded base64) and RFC 4648: 01 02 is `AQI`.
#[test]
fn response_metadata_reaches_the_outcome() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = unary_worker()?;
    let h = r.handle();
    let got = outcome(call(&h, grpc_request("meta"))?);
    drop(h);
    drop(r);

    assert_eq!(
        got,
        Some(GrpcOutcome {
            headers: fields(&[("x-h", "v"), ("x-b-bin", "AQI")]),
            trailers: fields(&[("x-t", "w")]),
            result: Ok("meta".into()),
        })
    );
    Ok(())
}

struct MetadataCase {
    name: &'static str,
    expected: &'static str,
}

// Expected values: ResponseMetadata.php (lower-case names, reserved names, the -bin rule, raw bytes) and its "dead with the call" docblock.
const METADATA_CASES: &[MetadataCase] = &[
    MetadataCase {
        name: "the accumulator is memoized",
        expected: "true",
    },
    MetadataCase {
        name: "an upper-case name is normalized",
        expected: "x-up",
    },
    MetadataCase {
        name: "a reserved name",
        expected: "ValueError",
    },
    MetadataCase {
        name: "-bin on addHeader",
        expected: "ValueError",
    },
    MetadataCase {
        name: "headers() holds raw bytes",
        expected: "0102",
    },
    MetadataCase {
        name: "add after respond",
        expected: "Rapira\\Exception\\AlreadyFinalizedError",
    },
    MetadataCase {
        name: "add after the call object is gone",
        expected: "Rapira\\Exception\\AlreadyFinalizedError",
    },
    MetadataCase {
        name: "headers() after the call object is gone",
        expected: "[]",
    },
];

#[test]
fn response_metadata_is_call_scoped() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = unary_worker()?;
    let h = r.handle();
    let got = outcome(call(&h, grpc_request("md-rules"))?);
    drop(h);
    drop(r);
    assert_eq!(got.map(|o| o.result), Some(Ok("md-rules".into())));

    let mut results = std::collections::HashMap::new();
    for record in captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "case")
    {
        let ctx: Value = serde_json::from_str(&record.context)?;
        let name = ctx["name"].as_str().unwrap_or_default().to_owned();
        let result = match &ctx["result"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        results.insert(name, result);
    }
    assert_eq!(
        results.len(),
        METADATA_CASES.len(),
        "one record per case (got {results:?})"
    );
    let mismatches: Vec<String> = METADATA_CASES
        .iter()
        .filter(|c| results.get(c.name).map(String::as_str) != Some(c.expected))
        .map(|c| {
            format!(
                "{}: expected {:?}, got {:?}",
                c.name,
                c.expected,
                results.get(c.name)
            )
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
    Ok(())
}

/// A dropped receiver is the host closing the call: Work.php (cancelled, finalized) and Responder.php (WorkDiscardedException, "The host closed the call first").
#[test]
fn cancel_is_visible_to_php() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = unary_worker()?;
    let h = r.handle();

    let rx = call(&h, grpc_request("cancel"))?;
    wait_app_record("got");
    drop(rx);
    let ctx: Value = serde_json::from_str(&wait_app_record("cancel"))?;
    drop(h);
    drop(r);

    assert_eq!(
        ctx,
        json!({
            "cancelled": true,
            "finalized": true,
            "respond": "Rapira\\Exception\\WorkDiscardedException: the host closed the call first",
        })
    );
    Ok(())
}

/// A call whose client left while it was queued is skipped at the pull.
#[test]
fn a_call_closed_before_the_pull_never_reaches_php() -> anyhow::Result<()> {
    let _guard = php_lock();
    let marker = std::env::temp_dir().join(format!("rapira-test-grpc-hold-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let r = unary_worker()?;
    let h = r.handle();

    let held = call(&h, grpc_request(&format!("hold:{}", marker.display())))?;
    wait_app_record("held");
    drop(call(&h, grpc_request("echo:gone"))?);
    let kept = call(&h, grpc_request("echo:kept"))?;
    std::fs::write(&marker, b"go")?;

    let held = outcome(held).map(|o| o.result);
    let kept = outcome(kept).map(|o| o.result);
    drop(h);
    drop(r);
    let _ = std::fs::remove_file(&marker);

    assert_eq!(held, Some(Ok("held".into())));
    assert_eq!(kept, Some(Ok("kept".into())));
    assert_eq!(
        results("echo"),
        ["kept"],
        "PHP must never see the closed call"
    );
    Ok(())
}

/// The boot probe runs on an empty intake, so tryReceive() gives null and frees a call object that holds no state.
#[test]
fn try_receive_on_an_empty_intake_returns_null() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = unary_worker()?;
    drop(r);
    assert_eq!(results("try"), ["NULL"]);
    Ok(())
}
