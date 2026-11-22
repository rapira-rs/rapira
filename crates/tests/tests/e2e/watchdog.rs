use std::io;
use std::time::{Duration, Instant};

use rapira_sapi::plugin::Mode;
use tests::{StatusBody, fixture};

use crate::harness::{Conn, Spawn, assert_exit_code, http_get, slot_line};

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
fn a_native_call_still_active_after_the_grace_exits_124() -> io::Result<()> {
    let mut srv = Spawn::http(Mode::Worker, fixture("watchdog/worker.php"))
        .http_pool("request_terminate_timeout_secs = 1")
        .php_ini("max_execution_time = 4\nmax_input_time = -1\nhard_timeout = 0")
        .spawn();
    let budget = Duration::from_secs(15);
    let mut conn = Conn::open(srv.addr, budget)?;
    let start = Instant::now();
    conn.send(b"GET /?derive HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")?;
    let exit = srv.wait_exit(budget);
    assert_exit_code(exit, 124, &srv);
    assert!(start.elapsed() >= Duration::from_secs(6), "early exit");
    let log = std::fs::read_to_string(srv.log_file())?;
    assert_eq!(log.matches("interrupting the request").count(), 1, "{log}");
    assert!(log.contains("exiting with code 124"), "{log}");
    Ok(())
}
