use bytes::Bytes;
use php_sys::{Mode, Rapira, grpc};
use serde_json::json;
use std::time::{Duration, Instant};
use tests::{captured, fixture, init_log_capture, php_lock};

fn mode(path: &str) -> Mode {
    Mode::GrpcDispatcher {
        entrypoint: fixture(path),
        services: vec![grpc::ServiceInfo {
            name: "example.Echo".into(),
            methods: vec![grpc::MethodInfo {
                name: "Probe".into(),
                input_type: "example.Input".into(),
                output_type: "example.Output".into(),
            }],
        }],
    }
}

fn request(case: &str) -> grpc::Request {
    grpc::Request {
        method: "example.Echo/Probe".into(),
        message: Bytes::from_static(b"\0\xff"),
        metadata: vec![
            ("test-case".into(), case.as_bytes().to_vec()),
            ("x-input-bin".into(), vec![0, 255]),
        ],
        remote: php_sys::types::Addr::Inet(([127, 0, 0, 1], 1234).into()),
        tls: None,
        received_at: 1234.5,
        deadline: None,
        expires_at: None,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

async fn wait_log(message: &str) {
    let end = Instant::now() + Duration::from_secs(10);
    while !captured()
        .iter()
        .any(|r| r.target == "app" && r.message == message)
    {
        assert!(Instant::now() < end, "missing PHP log {message}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn assert_counts(rapira: &Rapira, expected: (u64, u64, u64)) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = rapira.scoreboard();
        let actual = (snapshot.handled, snapshot.errors, snapshot.recycles);
        if actual == expected {
            return;
        }
        assert!(
            Instant::now() < end,
            "accounting did not complete: expected {expected:?}, got {actual:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn unary_dispatcher_bootstrap_context_and_response_ownership() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(mode("grpc/dispatcher.php"))?;
    let handle = rapira.handle();
    let rt = runtime();
    let reply = rt.block_on(handle.handle_grpc(request("inspect")))?;
    let actual: serde_json::Value = serde_json::from_slice(&reply.result.unwrap())?;
    assert_eq!(
        actual,
        json!({
            "bootstrap": ["Dispatcher", "grpc", "example.Echo", "Probe", "example.Input", "example.Output", "unary"],
            "context": ["example.Echo/Probe", "grpc", 1234.5, null, "Rapira\\InetAddress", "127.0.0.1", 1234, null, "00ff", "00ff"],
            "snapshots": [["one"], ["one", "two"], "00ff"],
            "busy": "Error",
            "active": 1,
            "explicit_destruct": false,
            "reserved": "ValueError",
            "binary_suffix": "ValueError",
            "text_suffix": "ValueError",
            "control": "ValueError",
            "http_control": "ValueError",
            "user_agent": "ValueError",
            "clone": "Error",
            "serialize": "Exception"
        })
    );
    assert_eq!(
        reply.headers,
        vec![
            ("x-order".into(), b"one".to_vec()),
            ("x-order".into(), b"two".to_vec()),
            ("x-binary-bin".into(), vec![0, 255])
        ]
    );
    assert_eq!(reply.trailers, vec![("x-end".into(), b"done".to_vec())]);

    let reply = rt.block_on(handle.handle_grpc(request("fail")))?;
    let status = reply.result.unwrap_err();
    assert_eq!((status.code, status.message.as_str()), (3, "bad % input"));
    assert_eq!(
        status.details[0].type_url,
        "type.googleapis.com/example.Detail"
    );
    assert_eq!(status.details[0].value.as_ref(), b"\0\xff");
    assert_eq!(reply.headers, vec![("x-error".into(), b"header".to_vec())]);
    assert_eq!(reply.trailers, vec![("x-error-bin".into(), vec![0, 255])]);

    struct Case {
        name: &'static str,
        reply: Result<&'static [u8], u8>,
        expected: serde_json::Value,
    }
    let cases = [
        Case {
            name: "retained",
            reply: Ok(b"\0\xff"),
            expected: json!([
                "Rapira\\Exception\\AlreadyFinalizedError",
                "Rapira\\Exception\\AlreadyFinalizedError",
                ["kept"],
                ["kept"],
                1
            ]),
        },
        Case {
            name: "abandoned",
            reply: Err(13),
            expected: json!(["Rapira\\Exception\\AlreadyFinalizedError", ["kept"], 1]),
        },
    ];
    for case in cases {
        let reply = rt.block_on(handle.handle_grpc(request(case.name)))?;
        assert_eq!(
            reply.result.as_deref().map_err(|status| status.code),
            case.reply,
            "{}",
            case.name
        );
        let report = rt.block_on(handle.handle_grpc(request("report")))?;
        let actual: serde_json::Value = serde_json::from_slice(&report.result.unwrap())?;
        assert_eq!(actual, case.expected, "{}", case.name);
    }
    assert_counts(&rapira, (6, 2, 0));
    drop(handle);
    rapira.shutdown();
    Ok(())
}

#[test]
fn exceptions_bailouts_and_exit_release_calls_then_recycle() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(mode("grpc/dispatcher.php"))?;
    let handle = rapira.handle();
    let rt = runtime();
    struct Case {
        name: &'static str,
    }
    for case in [
        Case { name: "throw" },
        Case { name: "bailout" },
        Case { name: "exit" },
    ] {
        let reply = rt.block_on(handle.handle_grpc(request(case.name)))?;
        let status = reply.result.unwrap_err();
        assert_eq!(status.code, 13, "{}", case.name);
        assert!(!status.message.contains("secret"));
        let reply = rt.block_on(handle.handle_grpc(request("echo")))?;
        assert_eq!(reply.result.unwrap().as_ref(), b"\0\xff", "{}", case.name);
    }
    assert_counts(&rapira, (6, 3, 3));
    drop(handle);
    rapira.shutdown();
    Ok(())
}

#[test]
fn failed_boot_returns_unavailable() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(mode("grpc/failboot.php"))?;
    let handle = rapira.handle();
    let reply = runtime().block_on(handle.handle_grpc(request("echo")))?;
    assert_eq!(reply.result.unwrap_err().code, 14);
    assert_counts(&rapira, (1, 1, 0));
    drop(handle);
    rapira.shutdown();
    Ok(())
}

#[test]
fn allocation_bailout_releases_native_snapshots_and_open_reply() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(mode("grpc/allocation.php"))?;
    let handle = rapira.handle();
    let rt = runtime();
    let mut input = request("allocate");
    input.message = Bytes::from(vec![b'x'; 4 * 1024 * 1024]);
    let failed = rt.block_on(handle.handle_grpc(input))?;
    assert_eq!(failed.result.unwrap_err().code, 13);
    let reply = rt.block_on(handle.handle_grpc(request("echo")))?;
    assert_eq!(reply.result.unwrap().as_ref(), b"next");
    assert_counts(&rapira, (2, 1, 1));
    drop(handle);
    rapira.shutdown();
    Ok(())
}

#[test]
fn message_buffer_preserves_php_string_ownership_and_content() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(mode("grpc/message_buffer.php"))?;
    let handle = rapira.handle();
    let rt = runtime();
    let a = json!([
        3145728,
        "6f850bc94ae6f7de14297c01616c36d712d22864497b28a63b81d776b035e656"
    ]);
    let b = json!([
        3145728,
        "6cac27e0f30e108ff9b437cf8d89933fbc66fff163267719cb005f3684c27f11"
    ]);
    let c = json!([
        2150400,
        "d97f50b810f51e4a21456057c6a904f8758129ed74138594be1e8278c040b12a"
    ]);
    let d = json!([
        4194304,
        "0020e43347537f3440ee8627f986e6ad02edec405d62d679ee53ecc75c5e2911"
    ]);
    struct Case {
        name: &'static str,
        action: &'static str,
        message: Vec<u8>,
        expected: serde_json::Value,
    }
    let cases = [
        Case {
            name: "binary_bytes",
            action: "inspect",
            message: b"\0\xffAz".repeat(1024 * 1024),
            expected: json!([
                4194304,
                "2f094434ba756bc5a8211fe86eb9ae72864d754172ff760437883d0286ad7826"
            ]),
        },
        Case {
            name: "retain_string",
            action: "retain_string",
            message: vec![b'a'; 3 * 1024 * 1024],
            expected: a.clone(),
        },
        Case {
            name: "retained_across_receive",
            action: "check_string",
            message: vec![b'b'; 3 * 1024 * 1024],
            expected: json!([b, a]),
        },
        Case {
            name: "retain_call",
            action: "retain_call",
            message: vec![b'a'; 3 * 1024 * 1024],
            expected: a.clone(),
        },
        Case {
            name: "retained_call_across_receive",
            action: "check_call",
            message: vec![b'b'; 3 * 1024 * 1024],
            expected: json!([b, a, b]),
        },
        Case {
            name: "caller_mutation",
            action: "mutate",
            message: vec![b'a'; 3 * 1024 * 1024],
            expected: json!([
                [
                    3145728,
                    "e1178e0307f308b4897ceed7679546d8b1e6fe344ff6c7c13d59ae569a87e112"
                ],
                a
            ]),
        },
        Case {
            name: "repeated_get",
            action: "repeated",
            message: vec![b'a'; 3 * 1024 * 1024],
            expected: json!([
                [
                    3145728,
                    "a341172195c64feb4853802b8314324d951a98d24e2ca44440131ee71aff1dd3"
                ],
                a,
                a
            ]),
        },
        Case {
            name: "shrinking_huge_message",
            action: "inspect",
            message: vec![b'c'; 2100 * 1024],
            expected: c,
        },
        Case {
            name: "growing_message",
            action: "inspect",
            message: vec![b'd'; 4 * 1024 * 1024],
            expected: d.clone(),
        },
        Case {
            name: "large_then_small",
            action: "inspect",
            message: b"\0\xffend\0".to_vec(),
            expected: json!([
                6,
                "47c6c0d9defc120aa244be9090cb2d55980824da6955955b724460ce30a73486"
            ]),
        },
        Case {
            name: "large_then_empty",
            action: "inspect",
            message: Vec::new(),
            expected: json!([
                0,
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            ]),
        },
        Case {
            name: "small_then_large",
            action: "inspect",
            message: vec![b'd'; 4 * 1024 * 1024],
            expected: d,
        },
        Case {
            name: "retained_payload_variable",
            action: "payload",
            message: vec![b'a'; 3 * 1024 * 1024],
            expected: a.clone(),
        },
        Case {
            name: "reassigned_payload_variable",
            action: "payload",
            message: vec![b'b'; 3 * 1024 * 1024],
            expected: b,
        },
        Case {
            name: "released_payload_variable",
            action: "release_payload",
            message: vec![b'a'; 3 * 1024 * 1024],
            expected: a,
        },
        Case {
            name: "derive_hash_and_utf8",
            action: "derived",
            message: vec![b'a'; 3 * 1024 * 1024],
            expected: json!(["found", 1, 0]),
        },
        Case {
            name: "clear_hash_and_utf8",
            action: "derived",
            message: vec![255; 3 * 1024 * 1024],
            expected: json!(["found", false, 4]),
        },
        Case {
            name: "derive_after_length_change",
            action: "derived",
            message: vec![b'c'; 2100 * 1024],
            expected: json!(["found", 1, 0]),
        },
    ];
    let count = cases.len() as u64;
    for case in cases {
        let mut input = request(case.action);
        input.message = Bytes::from(case.message);
        let reply = rt.block_on(handle.handle_grpc(input))?;
        let actual: serde_json::Value = serde_json::from_slice(&reply.result.unwrap())?;
        assert_eq!(actual, case.expected, "{}", case.name);
    }
    assert_counts(&rapira, (count, 0, 0));
    drop(handle);
    rapira.shutdown();
    Ok(())
}

#[test]
fn message_buffer_survives_failures_and_shutdown_callbacks() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();
    let rapira = Rapira::start(mode("grpc/message_buffer.php"))?;
    let handle = rapira.handle();
    let rt = runtime();
    let expected = json!([
        4194304,
        "0020e43347537f3440ee8627f986e6ad02edec405d62d679ee53ecc75c5e2911"
    ]);
    struct Case {
        name: &'static str,
        action: &'static str,
        len: usize,
        fails: bool,
    }
    let cases = [
        Case {
            name: "allocation_limit",
            action: "limit",
            len: 4 * 1024 * 1024,
            fails: true,
        },
        Case {
            name: "shared_allocation_limit",
            action: "limit_shared",
            len: 4 * 1024 * 1024,
            fails: true,
        },
        Case {
            name: "growth_allocation_limit",
            action: "limit",
            len: 8 * 1024 * 1024,
            fails: true,
        },
        Case {
            name: "exception",
            action: "throw",
            len: 4 * 1024 * 1024,
            fails: true,
        },
        Case {
            name: "fatal_bailout",
            action: "bailout",
            len: 4 * 1024 * 1024,
            fails: true,
        },
        Case {
            name: "exit",
            action: "exit",
            len: 4 * 1024 * 1024,
            fails: true,
        },
        Case {
            name: "shutdown_callback",
            action: "shutdown",
            len: 4 * 1024 * 1024,
            fails: false,
        },
        Case {
            name: "shutdown_bailout",
            action: "shutdown_bailout",
            len: 4 * 1024 * 1024,
            fails: false,
        },
        Case {
            name: "shutdown_allocation_limit",
            action: "shutdown_allocate",
            len: 4 * 1024 * 1024,
            fails: true,
        },
    ];
    let count = cases.len() as u64;
    let failures = cases.iter().filter(|case| case.fails).count() as u64;
    for case in cases {
        let mut input = request(case.action);
        input.message = Bytes::from(vec![b'd'; case.len]);
        let reply = rt.block_on(handle.handle_grpc(input))?;
        if case.fails {
            assert_eq!(reply.result.unwrap_err().code, 13, "{}", case.name);
        } else {
            let actual: serde_json::Value = serde_json::from_slice(&reply.result.unwrap())?;
            assert_eq!(actual, expected, "{}", case.name);
        }
        let mut next = request("inspect");
        next.message = Bytes::from(vec![b'd'; 4 * 1024 * 1024]);
        let reply = rt.block_on(handle.handle_grpc(next))?;
        let actual: serde_json::Value = serde_json::from_slice(&reply.result.unwrap())?;
        assert_eq!(actual, expected, "next call after {}", case.name);
    }
    assert_counts(&rapira, (2 * count, failures, count));
    drop(handle);
    rapira.shutdown();
    let logs = captured();
    let reports: Vec<_> = logs
        .iter()
        .filter(|record| record.message == "grpc-message-shutdown")
        .collect();
    assert_eq!(reports.len(), 2);
    for report in reports {
        let actual: serde_json::Value = serde_json::from_str(&report.context)?;
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[test]
fn canceled_and_expired_calls_release_receive_and_count_once() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();
    let rapira = Rapira::start(mode("grpc/dispatcher.php"))?;
    let handle = rapira.handle();
    runtime().block_on(async {
        wait_log("grpc-ready").await;
        let h = handle.clone();
        let active = tokio::spawn(async move { h.handle_grpc(request("busy")).await });
        wait_log("grpc-busy").await;
        active.abort();
        let _ = active.await;
        let h = handle.clone();
        let queued = tokio::spawn(async move { h.handle_grpc(request("echo")).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        queued.abort();
        let _ = queued.await;
        let mut expired = request("echo");
        expired.expires_at = Some(Instant::now() + Duration::from_millis(20));
        let reply = handle.handle_grpc(expired).await.unwrap();
        assert_eq!(reply.result.unwrap_err().code, 4);
        let reply = handle.handle_grpc(request("cancel-report")).await.unwrap();
        let actual: serde_json::Value = serde_json::from_slice(&reply.result.unwrap()).unwrap();
        assert_eq!(
            actual,
            json!([
                true,
                true,
                "Rapira\\Exception\\WorkDiscardedException",
                "Rapira\\Exception\\WorkDiscardedException",
                0,
                1
            ])
        );
    });
    assert_counts(&rapira, (4, 3, 0));
    drop(handle);
    rapira.shutdown();
    Ok(())
}

#[test]
fn deadline_expires_while_php_is_busy() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();
    let rapira = Rapira::start(mode("grpc/dispatcher.php"))?;
    let handle = rapira.handle();
    runtime().block_on(async {
        wait_log("grpc-ready").await;
        let mut req = request("busy");
        req.expires_at = Some(Instant::now() + Duration::from_millis(50));
        let start = Instant::now();
        let reply = handle.handle_grpc(req).await.unwrap();
        assert_eq!(reply.result.unwrap_err().code, 4);
        assert!(start.elapsed() < Duration::from_millis(250));
        let reply = handle.handle_grpc(request("cancel-report")).await.unwrap();
        assert!(reply.result.is_ok());
    });
    assert_counts(&rapira, (2, 1, 0));
    drop(handle);
    rapira.shutdown();
    Ok(())
}

#[test]
fn receive_modes_and_quota_share_the_worker_lifecycle() -> anyhow::Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let _guard = php_lock();
    init_log_capture();
    captured().clear();
    let module = Rapira::boot_master()?;
    let quota = Arc::new(AtomicUsize::new(0));
    let counter = quota.clone();
    let rapira = Rapira::start_worker(
        mode("grpc/receive.php"),
        php_sys::WorkerHooks {
            max_requests: 2,
            on_quota: Some(Box::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            })),
            ..Default::default()
        },
    )?;
    let handle = rapira.handle();
    runtime().block_on(async {
        wait_log("grpc-receive").await;
        struct Case {
            name: &'static str,
            message: &'static [u8],
        }
        for case in [
            Case {
                name: "empty",
                message: b"",
            },
            Case {
                name: "binary",
                message: b"\0\xff",
            },
        ] {
            let mut req = request(case.name);
            req.message = Bytes::from_static(case.message);
            let reply = handle.handle_grpc(req).await.unwrap();
            assert_eq!(
                reply.result.unwrap().as_ref(),
                case.message,
                "{}",
                case.name
            );
        }
    });
    drop(handle);
    rapira.shutdown();
    drop(module);
    assert_eq!(quota.load(Ordering::Relaxed), 1);
    let logs = captured();
    let report = logs.iter().find(|r| r.message == "grpc-receive").unwrap();
    let actual: serde_json::Value = serde_json::from_str(&report.context)?;
    assert_eq!(
        actual,
        json!({"try": null, "zero": "Rapira\\Exception\\TimeoutException", "finite": "Rapira\\Exception\\TimeoutException", "negative": "ValueError"})
    );
    assert_eq!(
        logs.iter().filter(|r| r.message == "grpc-closed").count(),
        1
    );
    Ok(())
}
