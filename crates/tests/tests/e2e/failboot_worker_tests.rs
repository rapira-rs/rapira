use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use rapira_sapi::plugin::Mode;
use tests::grpc::{Conn, ECHO_PATH, HI_FRAME, Wire, status_details};
use tests::wire::submit;
use tests::{drain_resp_deadline, fixture, poll, req};

use crate::harness::{
    Exited, MASTER_EXIT_FAILBOOT, MASTER_EXIT_OK, Spawn, assert_exit_code, diagnostics, listen,
    signal, wait_log_contains,
};

// A worker with a fatal error before its receive loop must answer the queued unit with 503, and the SIGQUIT stop must not wait for a boot that retries forever.
#[test]
fn failboot_worker_serves_503_and_drops_cleanly() -> anyhow::Result<()> {
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture("failboot_worker_tests/failboot-worker.php"),
    )
    // No failure limit: with the default limit, the server stops 7.5 s after the first failed boot.
    .http_pool("max_boot_failures = 0")
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
    // No failure limit: with the default limit, the server stops 7.5 s after the first failed boot.
    let srv = Spawn::grpc(fixture("failboot_worker_tests/failboot-worker.php"))
        .grpc_pool("max_boot_failures = 0")
        .spawn();
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

// A request during the wait after a failed boot gets 503 and starts no boot. The next boot starts when the wait ends. The 5 failed boots of the default max_boot_failures have 4 waits between them (0.5, 1, 2 and 4 s), so the 4 requests do not shorten the time to the unhealthy flag. The flag stops the worker, and the master stops with exit code 70 because the pool never served.
#[test]
fn requests_during_the_boot_wait_get_503_and_start_no_boot() -> anyhow::Result<()> {
    let started = Instant::now();
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture("failboot_worker_tests/failboot-worker.php"),
    )
    .spawn();
    let deadline = Instant::now() + Duration::from_secs(20);

    let mut statuses = Vec::new();
    for _ in 0..4 {
        let mut rx = submit(srv.addr, req("/"))?;
        let resp = drain_resp_deadline(&mut rx, deadline)
            .expect("boot-failing worker hung (unhealthy regression)");
        statuses.push(resp.status());
    }
    assert_eq!(statuses, [503; 4], "each request during the wait gets 503");

    let status = srv.wait_exit(deadline.saturating_duration_since(Instant::now()));
    assert_exit_code(status, MASTER_EXIT_FAILBOOT, &srv);
    let took = started.elapsed();
    let log = std::fs::read_to_string(srv.log_file())?;
    assert!(
        log.contains("worker keeps failing to boot; flagged unhealthy"),
        "5 failed boots in sequence must flag the worker unhealthy\n{log}"
    );
    assert!(
        took >= Duration::from_millis(7500),
        "the requests started boots: the master stopped after {took:?}\n{log}"
    );
    Ok(())
}

// A pool whose boot always fails stops the master with exit code 70 after max_boot_failures failed boots in sequence, 5 by default. The worker waits between the boots: 500 ms after the first failed boot, and each further failed boot doubles the wait.
#[test]
fn a_pool_that_never_boots_gives_up_after_max_boot_failures() {
    struct Case {
        name: &'static str,
        /// Keys inside `[http.pool]`.
        pool: &'static str,
        /// The boots that the log holds before the unhealthy record.
        boots: usize,
        /// The sum of the waits between these boots.
        waits: Duration,
    }
    let cases = [
        Case {
            name: "the default limit",
            pool: "",
            boots: 5,
            waits: Duration::from_millis(7500),
        },
        Case {
            name: "the smallest limit",
            pool: "max_boot_failures = 1",
            boots: 1,
            waits: Duration::ZERO,
        },
    ];
    for case in cases {
        let started = Instant::now();
        let Exited { status, log } = Spawn::http(
            Mode::Worker,
            fixture("failboot_worker_tests/logged-failboot-worker.php"),
        )
        .http_pool(case.pool)
        .boot_failure();
        let took = started.elapsed();
        assert_eq!(
            status.code(),
            Some(MASTER_EXIT_FAILBOOT),
            "{}\n{log}",
            case.name
        );
        let Some((before, _)) = log.split_once("worker keeps failing to boot; flagged unhealthy")
        else {
            panic!("{}: the worker stays healthy\n{log}", case.name);
        };
        assert_eq!(
            before.matches("boot attempt").count(),
            case.boots,
            "{}\n{log}",
            case.name
        );
        assert!(
            took >= case.waits,
            "{}: the master stopped after {took:?}\n{log}",
            case.name
        );
    }
}

// max_boot_failures = 0 is no limit. After 6 failed boots, one more than the default limit, the worker is not flagged, a request still gets 503, and the master runs.
#[test]
fn max_boot_failures_zero_keeps_a_worker_that_never_boots() -> anyhow::Result<()> {
    let mut srv = Spawn::http(
        Mode::Worker,
        fixture("failboot_worker_tests/logged-failboot-worker.php"),
    )
    .http_pool("max_boot_failures = 0")
    .spawn();
    let log = srv.log_file();
    let text = poll(Duration::from_secs(30), || {
        let text = std::fs::read_to_string(&log).ok()?;
        (text.matches("boot attempt").count() >= 6).then_some(text)
    })
    .unwrap_or_else(|| panic!("fewer than 6 boots\n{}", diagnostics(&srv)));
    assert!(!text.contains("flagged unhealthy"), "\n{text}");

    let mut rx = submit(srv.addr, req("/"))?;
    let resp = drain_resp_deadline(&mut rx, Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| panic!("no response\n{}", diagnostics(&srv)));
    assert_eq!(resp.status(), 503, "\n{}", diagnostics(&srv));
    assert_eq!(
        srv.try_status(),
        None,
        "the master keeps running\n{}",
        diagnostics(&srv)
    );
    Ok(())
}

// An idle keep-alive connection holds the drain of an unhealthy worker open for the whole drain grace. The worker boots no more after the unhealthy flag: it sheds the units that arrive and exits when the drain ends. process_control_timeout_secs = 4 gives a drain grace of 2 s, longer than the 1 s boot wait after the second failed boot.
#[test]
fn an_unhealthy_worker_boots_no_more_while_a_connection_holds_its_drain() -> anyhow::Result<()> {
    let mut srv = Spawn::http(
        Mode::Worker,
        fixture("failboot_worker_tests/logged-failboot-worker.php"),
    )
    .http_pool("max_boot_failures = 2")
    .toml("[supervisor]\nprocess_control_timeout_secs = 4")
    .spawn();
    let read = Duration::from_secs(10);
    // The first boot record starts the 500 ms wait, so the GET arrives before the second failed boot flags the worker.
    assert!(
        wait_log_contains(&srv, "boot attempt", read),
        "no boot\n{}",
        diagnostics(&srv)
    );
    // The connection stays open until the end of the test.
    let mut conn = crate::harness::Conn::open(srv.addr, read)?;
    conn.send(b"GET / HTTP/1.1\r\nHost: e2e\r\n\r\n")?;
    let head = conn.read_head(read)?;
    assert_eq!(
        (
            head.status,
            head.fields.iter().find(|(k, _)| k == "connection")
        ),
        (503, None),
        "a kept-alive 503 of the boot wait\n{}",
        diagnostics(&srv)
    );

    let status = srv.wait_exit(Duration::from_secs(20));
    assert_exit_code(status, MASTER_EXIT_FAILBOOT, &srv);
    let log = std::fs::read_to_string(srv.log_file())?;
    let Some((_, after)) = log.split_once("worker keeps failing to boot; flagged unhealthy") else {
        panic!("the worker stays healthy\n{log}");
    };
    assert_eq!(
        after.matches("boot attempt").count(),
        0,
        "boots after the unhealthy flag\n{log}"
    );
    Ok(())
}
