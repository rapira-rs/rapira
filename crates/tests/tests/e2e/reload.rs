use crate::harness::*;
use rapira_sapi::plugin::Mode;
use std::time::Duration;
use tests::poll;
use tests::server_log::{app_contexts, wait_app_record};

/// USR2 replaces every worker while the master keeps the listener bound; connection drops during the swap are the load balancer's concern, served responses are ours.
#[test]
fn usr2_reload_replaces_workers_while_serving() {
    let srv = spawn_with_config("shared/echo-worker.php", 3, "");
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "3 workers", |p| p.len() == 3);
    let storm = storm(srv.addr, 8);
    std::thread::sleep(Duration::from_millis(500));
    signal(srv.pid(), libc::SIGUSR2);
    wait_workers(
        &srv,
        Duration::from_secs(60),
        "reload to 3 disjoint pids",
        |p| p.len() == 3 && p.iter().all(|x| !pids0.contains(x)),
    );
    std::thread::sleep(Duration::from_secs(2));
    let tally = storm.halt();

    assert_eq!(
        tally.refused,
        0,
        "the listener must stay bound across the reload; last error: {:?}\n{}",
        tally.last_err,
        diagnostics(&srv),
    );
    assert!(tally.ok > 0, "storm served no requests during the reload");
    assert_eq!(
        tally.failed,
        0,
        "requests failed beyond tolerated connection drops; last error: {:?}\n{}",
        tally.last_err,
        diagnostics(&srv),
    );
}

/// The first replacement of a reload takes the place of the last old worker. When that worker exits on its own before the replacement serves, for example by an OOM kill, the pool still has `processes` workers after the reload.
#[test]
fn reload_keeps_the_worker_count_when_the_last_old_worker_dies() {
    let srv = spawn_with_config("lifecycle/slow-boot-worker.php", 1, "");
    let old = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET /")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));

    signal(srv.pid(), libc::SIGUSR2);
    wait_workers(&srv, Duration::from_secs(20), "the replacement", |p| {
        p.len() == 2
    });
    signal(old, libc::SIGKILL);
    // The master reaps a worker after all its threads exit. Before that, the blocked accept thread of the old worker can take the EPOLLEXCLUSIVE wakeup of the next connection.
    let reaped = format!("http pool: worker {old} killed by signal {}", libc::SIGKILL);
    assert!(
        wait_log_contains(&srv, &reaped, Duration::from_secs(5)),
        "no {reaped:?} record\n{}",
        diagnostics(&srv)
    );
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET / after the kill")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    wait_workers(
        &srv,
        Duration::from_secs(5),
        "1 worker after the reload",
        |p| p.len() == 1,
    );
}

/// SIGUSR2 to a worker that waits for work restarts its app without a request, in the same process.
#[test]
fn usr2_restarts_an_idle_app() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/boot-log-worker.php"),
    )
    .json_log()
    .spawn();
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    let log = srv.log_file();
    wait_app_record(&log, "booted");
    signal(pid, libc::SIGUSR2);
    let booted = poll(Duration::from_secs(10), || {
        (app_contexts(&log, "booted").len() >= 2).then_some(())
    });
    assert!(
        booted.is_some(),
        "the idle app booted again\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        worker_pids(srv.pid()),
        vec![pid],
        "the worker process stays\n{}",
        diagnostics(&srv)
    );
}
