use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use rapira_sapi::plugin::Mode;
use tests::grpc::{Conn, ECHO_PATH, HI_FRAME, Wire, status_details};
use tests::wire::submit;
use tests::{drain_resp_deadline, fixture, req};

use crate::harness::{
    MASTER_EXIT_FAILBOOT, MASTER_EXIT_OK, Spawn, assert_exit_code, diagnostics, listen, signal,
    wait_log_contains, wait_workers, worker_pids,
};

// A worker that fatals before its receive loop must 503 the queued unit, and the graceful stop must not wait on a boot that retries forever.
#[test]
fn failboot_worker_serves_503_and_drops_cleanly() -> anyhow::Result<()> {
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture("failboot_worker_tests/failboot-worker.php"),
    )
    .spawn();
    let deadline = Instant::now() + Duration::from_secs(10);

    let mut rx = submit(srv.addr, req("/"))?;
    let resp = drain_resp_deadline(&mut rx, deadline)
        .expect("broken worker black-holed the request (A6 regression)");
    assert_eq!(
        resp.status(),
        503,
        "a boot-failed worker must 503 the queued unit"
    );

    signal(srv.pid(), libc::SIGQUIT);
    let status = srv.wait_exit(deadline.saturating_duration_since(Instant::now()));
    assert_exit_code(status, MASTER_EXIT_OK, &srv);
    Ok(())
}

// A gRPC worker that fatals before its receive loop sheds the queued call with UNAVAILABLE, as the HTTP worker sheds with 503.
#[test]
fn failboot_grpc_worker_sheds_with_unavailable() -> anyhow::Result<()> {
    let srv = Spawn::grpc(fixture("failboot_worker_tests/failboot-worker.php")).spawn();
    let listen = listen(&srv);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let got = rt
        .block_on(async {
            let call = async {
                let mut conn = Conn::open(&listen, Wire::H2).await?;
                conn.grpc(ECHO_PATH, &[], HI_FRAME).await
            };
            tokio::time::timeout(Duration::from_secs(10), call).await
        })
        .expect("no outcome within 10 s")?;

    let message = "the worker failed to boot";
    // google.rpc.Status: code (field 1, varint) 14, message (field 2, length-delimited), no details (field 3).
    let mut status = vec![0x08, 14, 0x12, message.len() as u8];
    status.extend_from_slice(message.as_bytes());
    // The trailers carry the status and no metadata.
    let names: BTreeSet<&str> = got.trailers.keys().map(|k| k.as_str()).collect();
    assert_eq!(
        (
            got.status,
            got.trailers.get("grpc-status").map(|v| v.as_bytes()),
            got.trailers.get("grpc-message").map(|v| v.as_bytes()),
            status_details(&got.trailers),
            names,
        ),
        (
            200,
            Some(&b"14"[..]),
            Some(message.as_bytes()),
            Some(status),
            BTreeSet::from(["grpc-message", "grpc-status", "grpc-status-details-bin"]),
        ),
        "got {got:?}"
    );
    Ok(())
}

// UNHEALTHY_AFTER (5) consecutive boot failures must flag the worker unhealthy, each failed boot 503ing its queued unit.
// The boot runs failed cycle 1. The 4 requests arrive before the 5 s boot retry, so each shed unit starts the next cycle, and cycle 5 follows unit 4 and flags the worker. The flag stops the worker, and the master failboots because the gen-0 pool never served.
#[test]
fn failboot_worker_flags_unhealthy_after_threshold() -> anyhow::Result<()> {
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture("failboot_worker_tests/failboot-worker.php"),
    )
    .spawn();
    let deadline = Instant::now() + Duration::from_secs(15);

    let mut statuses = Vec::new();
    for _ in 0..4 {
        let mut rx = submit(srv.addr, req("/"))?;
        let resp = drain_resp_deadline(&mut rx, deadline)
            .expect("boot-failing worker hung (unhealthy regression)");
        statuses.push(resp.status());
    }
    assert!(
        statuses.iter().all(|&s| s == 503),
        "each boot-failed unit must 503 (got {statuses:?})"
    );

    let status = srv.wait_exit(deadline.saturating_duration_since(Instant::now()));
    assert_exit_code(status, MASTER_EXIT_FAILBOOT, &srv);
    let log = std::fs::read_to_string(srv.log_file())?;
    assert!(
        log.contains("worker keeps failing to boot; flagged unhealthy"),
        "5 consecutive boot failures must flag the worker unhealthy\n{log}"
    );
    Ok(())
}

// An app instance that takes one unit and then ends with a non-zero exit status is a failed cycle; a fatal error sets status 255. UNHEALTHY_AFTER (5) failed cycles in a row flag the worker. An app instance that takes a second unit resets the count.
// The app took units, so the pool booted: the master replaces the flagged worker and keeps running.
#[test]
fn an_app_that_fails_at_its_first_unit_flags_the_worker_unhealthy() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        /// The status of each request, in the order sent.
        statuses: &'static [u16],
        /// The worker is flagged unhealthy after the last response.
        flagged: bool,
    }
    let cases = [
        Case {
            name: "the app answers 500, then the entrypoint ends with exit status 1",
            fixture: "failboot_worker_tests/exit-at-first-unit-worker.php",
            statuses: &[500, 500, 500, 500, 500],
            flagged: true,
        },
        Case {
            name: "a fatal error ends the app instance at its first unit",
            fixture: "failboot_worker_tests/fatal-at-first-unit-worker.php",
            statuses: &[500, 500, 500, 500, 500],
            flagged: true,
        },
        Case {
            name: "the app serves one unit, then fails at its second unit",
            fixture: "failboot_worker_tests/exit-at-second-unit-worker.php",
            // 6 failed cycles. The last 200 comes from cycle 7, so cycle 6 ended before it.
            statuses: &[
                200, 500, 200, 500, 200, 500, 200, 500, 200, 500, 200, 500, 200,
            ],
            flagged: false,
        },
    ];
    let record = "worker keeps failing at its first unit; flagged unhealthy";
    for case in cases {
        let mut srv = Spawn::http(Mode::Worker, fixture(case.fixture)).spawn();
        let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
        let deadline = Instant::now() + Duration::from_secs(15);

        let mut statuses = Vec::new();
        for _ in case.statuses {
            let mut rx = submit(srv.addr, req("/"))?;
            let resp = drain_resp_deadline(&mut rx, deadline)
                .unwrap_or_else(|| panic!("{}: no response\n{}", case.name, diagnostics(&srv)));
            statuses.push(resp.status());
        }
        assert_eq!(
            statuses,
            case.statuses,
            "{}\n{}",
            case.name,
            diagnostics(&srv)
        );

        if case.flagged {
            assert!(
                wait_log_contains(&srv, record, Duration::from_secs(10)),
                "{}: the worker stays healthy\n{}",
                case.name,
                diagnostics(&srv)
            );
            wait_workers(&srv, Duration::from_secs(20), "a replacement worker", |p| {
                p.len() == 1 && p[0] != pid
            });
            assert_eq!(
                srv.try_status(),
                None,
                "{}: the master keeps running\n{}",
                case.name,
                diagnostics(&srv)
            );
        } else {
            let log = std::fs::read_to_string(srv.log_file())?;
            assert!(!log.contains("flagged unhealthy"), "{}\n{log}", case.name);
            assert_eq!(
                worker_pids(srv.pid()),
                vec![pid],
                "{}: the worker process stays\n{}",
                case.name,
                diagnostics(&srv)
            );
        }
    }
    Ok(())
}
