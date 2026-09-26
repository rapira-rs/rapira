use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use rapira_net::ListenAddr;
use rapira_sapi::Mode;
use tests::grpc::{Conn, ECHO_PATH, HI_FRAME, Wire, status_details};
use tests::wire::submit;
use tests::{drain_resp_deadline, fixture, req};

use crate::harness::{MASTER_EXIT_FAILBOOT, MASTER_EXIT_OK, Spawn, assert_exit_code, signal};

// A worker that fatals before its receive loop must 503 the queued job, and the graceful stop must not wait on a boot that retries forever.
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
        "a boot-failed worker must 503 the queued job"
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
    let listen = ListenAddr::Tcp(srv.grpc.expect("the grpc listener"));
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

// UNHEALTHY_AFTER (5) consecutive boot failures must flag the worker unhealthy, each failed boot 503ing its queued job.
// The boot runs failed cycle 1, and each shed job starts the next cycle. So cycle 5 follows job 4 and flags the worker. The flag stops the worker, and the master failboots because the gen-0 pool never served.
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
        "each boot-failed job must 503 (got {statuses:?})"
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
