#![cfg(not(any(target_os = "macos", target_os = "windows")))]

use php_sys::{Mode, Rapira, grpc};
use std::path::Path;
use std::time::{Duration, Instant};
use tests::{captured, drain, fixture, init_log_capture, php_lock_with_ini, req};

#[test]
fn empty_receive_outcomes_restore_the_execution_budget() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        grpc: bool,
        action: &'static str,
        outcome: &'static str,
    }
    let cases = [
        Case {
            name: "http_poll",
            grpc: false,
            action: "poll",
            outcome: "empty",
        },
        Case {
            name: "grpc_poll",
            grpc: true,
            action: "poll",
            outcome: "empty",
        },
        Case {
            name: "http_zero_timeout",
            grpc: false,
            action: "zero",
            outcome: "timeout",
        },
        Case {
            name: "grpc_zero_timeout",
            grpc: true,
            action: "zero",
            outcome: "timeout",
        },
        Case {
            name: "http_finite_timeout",
            grpc: false,
            action: "finite",
            outcome: "timeout",
        },
        Case {
            name: "grpc_finite_timeout",
            grpc: true,
            action: "finite",
            outcome: "timeout",
        },
        Case {
            name: "http_closed",
            grpc: false,
            action: "closed",
            outcome: "closed",
        },
        Case {
            name: "grpc_closed",
            grpc: true,
            action: "closed",
            outcome: "closed",
        },
    ];
    let _guard = php_lock_with_ini(&fixture("ini/timeout_tests/timeout.php.ini"));
    init_log_capture();
    let rt = tokio::runtime::Builder::new_current_thread().build()?;
    for case in cases {
        captured().clear();
        let entrypoint = fixture("timeout_tests/receive-worker.php");
        let mode = if case.grpc {
            Mode::GrpcDispatcher {
                entrypoint,
                services: Vec::new(),
            }
        } else {
            Mode::Dispatcher(entrypoint)
        };
        let rapira = Rapira::start(mode)?;
        let handle = rapira.handle();
        if case.grpc {
            let reply = rt.block_on(handle.handle_grpc(grpc::Request {
                method: "example.Timer/Probe".into(),
                message: bytes::Bytes::from_static(case.action.as_bytes()),
                metadata: Vec::new(),
                remote: php_sys::types::Addr::Inet(([127, 0, 0, 1], 1234).into()),
                tls: None,
                received_at: 0.0,
                deadline: None,
                expires_at: None,
            }))?;
            assert_eq!(reply.result.unwrap().as_ref(), b"ready", "{}", case.name);
        } else {
            let (status, body) = drain(handle.handle_blocking(req(
                &format!("/{}", case.action),
                "timeout_tests/receive-worker.php",
            ))?);
            assert_eq!((status, body.as_str()), (200, "ready"), "{}", case.name);
        }
        if case.action != "closed" {
            let end = Instant::now() + Duration::from_secs(10);
            while !captured()
                .iter()
                .any(|r| r.target == "app" && r.message == "receive-timer-finished")
            {
                assert!(Instant::now() < end, "{} did not finish", case.name);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        drop(handle);
        rapira.shutdown();
        let records = captured();
        let outcome = records
            .iter()
            .find(|r| r.target == "app" && r.message == "receive-timer-outcome")
            .expect("receive outcome");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&outcome.context)?,
            serde_json::json!({"outcome": case.outcome}),
            "{}",
            case.name
        );
        assert!(
            records.iter().any(|r| r.target == "php"
                && r.level == tracing::Level::ERROR
                && r.message.contains("Maximum execution time")),
            "{} did not report an execution timeout: {records:#?}",
            case.name
        );
        assert!(
            !records
                .iter()
                .any(|r| r.target == "app" && r.message == "receive-timer-survived"),
            "{} exceeded the execution budget",
            case.name
        );
    }
    Ok(())
}

/// Pins that receive() disarms the wall timer while parked: a worker parked past the 1s budget still serves instead of fataling and being 503-shed.
#[test]
fn parked_receive_outlives_the_execution_budget() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/echo-loop-worker.php")))?;
    let h = r.handle();

    let (status, body) =
        drain(h.handle_blocking(req("/warmup", "dispatcher/echo-loop-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "method=GET body="));

    for target in ["/first", "/second"] {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (status, body) =
            drain(h.handle_blocking(req(target, "dispatcher/echo-loop-worker.php"))?);
        assert_eq!(
            (status, body.as_str()),
            (200, "method=GET body="),
            "a worker parked past the budget must still serve {target}"
        );
    }

    drop(h);
    r.shutdown();
    Ok(())
}

/// Pins that the budget re-armed at unit handout still fires: a spinning unit is killed with its response unsealed and the recycled worker keeps serving.
#[test]
fn rearmed_budget_kills_a_spinning_unit() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle();

    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/verbs-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "state=false"));

    let mut rx = h.handle_blocking(req("/?probe=spin", "dispatcher/verbs-worker.php"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let resp = tests::drain_resp_deadline(&mut rx, deadline)
        .expect("spinning unit was never killed - the per-unit budget did not re-arm");
    assert!(
        resp.head.is_none() && !resp.ended,
        "a spinning unit must not seal a response (got status {})",
        resp.status()
    );

    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/verbs-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must recover after the timeout"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// Pins that the per-job re-arm with reset_signals=0 still delivers SIGRTMIN: a spin on a later job in the same cycle is killed, not left running.
#[test]
fn max_execution_time_fires_on_rearmed_jobs() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Worker(fixture("timeout_tests/timeout-worker.php")))?;
    let h = r.handle();

    let (status, body) = drain(h.handle_blocking(req(
        "/timeout-worker.php",
        "timeout_tests/timeout-worker.php",
    ))?);
    assert_eq!((status, body.as_str()), (200, "ok"));

    let mut rx = h.handle_blocking(req(
        "/timeout-worker.php?mode=spin",
        "timeout_tests/timeout-worker.php",
    ))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let resp = tests::drain_resp_deadline(&mut rx, deadline)
        .expect("spinning job was never killed - max_execution_time did not fire");
    assert!(resp.ended, "worker died without sealing a response");
    let body = resp.body_string();
    assert!(
        body.contains("Maximum execution time"),
        "the timeout fatal must reach the body (got: {body:?})"
    );
    assert_eq!(resp.status(), 200);

    let (status, body) = drain(h.handle_blocking(req(
        "/timeout-worker.php",
        "timeout_tests/timeout-worker.php",
    ))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "ok"),
        "the worker must recover after a timeout"
    );

    drop(h);
    r.shutdown();
    Ok(())
}
