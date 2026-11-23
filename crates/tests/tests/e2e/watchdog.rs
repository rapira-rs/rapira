use std::io;
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use rapira_config::Mode;
use tests::{StatusBody, fixture, poll, server_log};

use crate::harness::{
    Conn, Spawn, assert_exit_code, fixture_path, http_get, listen, slot_line, wait_log_contains,
    wait_serving,
};

const WAIT: Duration = Duration::from_secs(15);
const BLOCKED: &str = "the unit will end when its native call returns";

#[cfg(target_os = "linux")]
#[test]
fn only_a_configured_watchdog_requires_a_free_wake_signal() {
    // The child inherits this thread's mask. Other tests keep their own masks.
    std::thread::spawn(|| {
        // SAFETY: sigemptyset initializes set. pthread_sigmask copies it.
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGRTMAX());
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()),
                0
            );
        }
        let spawn = || Spawn::http(Mode::Worker, fixture("watchdog/worker.php"));
        let untimed = spawn().spawn();
        let response = http_get(untimed.addr, "/", WAIT).expect("request without a watchdog");
        assert_eq!(
            (response.status, response.body.as_slice()),
            (200, b"ok".as_slice())
        );
        drop(untimed);

        let refused = spawn()
            .http_pool("request_terminate_timeout_secs = 2")
            .boot_failure();
        assert_eq!(refused.status.code(), Some(1), "{}", refused.log);
        assert!(refused.log.contains("SIGRTMAX"), "{}", refused.log);
    })
    .join()
    .unwrap();
}

#[test]
fn a_request_timeout_recycles_the_app_and_the_next_request_succeeds() -> io::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("watchdog/worker.php"))
        .http_pool("request_terminate_timeout_secs = 2")
        .php_ini("display_errors = Off\nmax_execution_time = 0\nmax_input_time = -1")
        .spawn();
    let budget = Duration::from_secs(15);
    let start = Instant::now();
    let response = http_get(srv.addr, "/?sleep", budget);
    assert!(
        start.elapsed() < budget,
        "the client timed out: {response:?}"
    );
    assert!(
        matches!(&response, Err(_) | Ok(StatusBody { status: 500, .. })),
        "the timed out request got {response:?}"
    );

    let next = http_get(srv.addr, "/", budget)?;
    assert_eq!((next.status, next.body.as_slice()), (200, b"ok".as_slice()));
    slot_line(&srv, " recycles=1 ");
    Ok(())
}

#[test]
fn a_pool_with_no_worker_left_after_the_grace_exits_124() -> io::Result<()> {
    let mut spawn = Spawn::http(Mode::Worker, fixture("watchdog/worker.php"))
        .http_pool("request_terminate_timeout_secs = 1")
        .php_ini("max_execution_time = 4\nmax_input_time = -1\nhard_timeout = 0")
        .json_log();
    if cfg!(php_zts) {
        spawn = spawn.with_grpc(fixture_path("grpc/echo-worker.php"));
    }
    let mut srv = spawn.spawn();
    let _peer = UnixListener::bind(srv.dir.join("peer.sock"))?;
    let mut conn = Conn::open(srv.addr, WAIT)?;
    let start = Instant::now();
    conn.send(b"GET /?read HTTP/1.0\r\n\r\n")?;
    let exit = srv.wait_exit(WAIT);
    assert_exit_code(exit, 124, &srv);
    assert!(start.elapsed() >= Duration::from_secs(6), "early exit");
    let records = server_log::records(&srv.log_file());
    let errors: Vec<_> = records
        .iter()
        .filter(|r| r.level == tracing::Level::ERROR && r.target == "rapira")
        .map(|r| r.message.as_str())
        .collect();
    assert_eq!(
        records
            .iter()
            .filter(|r| r.message.contains("interrupting the request"))
            .count(),
        1
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(
        errors[0].contains("http worker 0") && errors[0].contains(BLOCKED),
        "{errors:?}"
    );
    assert!(
        errors[1].contains("every worker of pool http is blocked; exiting with code 124"),
        "{errors:?}"
    );
    Ok(())
}

#[tokio::test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
async fn a_blocked_worker_recovers_while_its_sibling_and_another_pool_serve() -> anyhow::Result<()>
{
    use tests::grpc::{Conn as Grpc, ECHO_PATH, HI_FRAME, Wire};

    let srv = Spawn::http(Mode::Worker, fixture("watchdog/worker.php"))
        .with_grpc(fixture_path("grpc/echo-worker.php"))
        .workers(2)
        .http_pool("request_terminate_timeout_secs = 1")
        .php_ini("display_errors=Off\nmax_execution_time=0\nmax_input_time=-1")
        .json_log()
        .spawn();
    wait_serving(&srv, "http", 2);
    let peer = tokio::net::UnixListener::bind(srv.dir.join("peer.sock"))?;
    let mut slow = Conn::open(srv.addr, WAIT)?;
    slow.send(b"GET /?read HTTP/1.0\r\n\r\n")?;
    let (held, _) = tokio::time::timeout(WAIT, peer.accept()).await??;
    assert!(
        wait_log_contains(&srv, BLOCKED, WAIT),
        "{}",
        std::fs::read_to_string(srv.log_file())?
    );
    let sibling = http_get(srv.addr, "/?app", WAIT)?;
    assert_eq!(sibling.status, 200);
    let echo = tokio::time::timeout(WAIT, async {
        let mut grpc = Grpc::open(&listen(&srv), Wire::H2).await?;
        grpc.grpc(ECHO_PATH, &[], HI_FRAME).await
    })
    .await??;
    assert_eq!(echo.grpc_status(), Some("0"));
    assert_eq!(&echo.body[..], HI_FRAME);
    // Keep the same unit through another watchdog tick. It must log its blocked state only once.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    drop(held);
    assert_eq!(slow.read_head(WAIT)?.status, 500);
    // A different app token proves the worker that was blocked served again.
    poll(WAIT, || {
        let next = http_get(srv.addr, "/?app", Duration::from_secs(1)).ok()?;
        (next.status == 200 && next.body.len() == 16 && next.body != sibling.body).then_some(())
    })
    .expect("the recovered app served no request");
    let records = server_log::records(&srv.log_file());
    assert_eq!(
        records
            .iter()
            .filter(|r| r.level == tracing::Level::ERROR && r.message.contains(BLOCKED))
            .count(),
        1
    );
    assert!(
        records
            .iter()
            .any(|r| r.target == "php" && r.message.contains("Maximum execution time"))
    );
    Ok(())
}
