use crate::harness::*;
use rapira_sapi::plugin::Mode;
use std::time::Duration;
use tests::poll;
use tests::server_log::{app_contexts, wait_app_record};

/// USR2 restarts the app inside every worker: the listener and the worker processes stay, and no request fails.
#[test]
fn usr2_reload_restarts_the_apps_while_serving() {
    let srv = spawn_with_config(
        "lifecycle/boot-log-worker.php",
        3,
        "[log]\nformat = \"json\"\n",
    );
    let mut pids = wait_workers(&srv, Duration::from_secs(20), "3 workers", |p| p.len() == 3);
    pids.sort();
    let storm = storm(srv.addr, 8);
    std::thread::sleep(Duration::from_millis(500));
    signal(srv.pid(), libc::SIGUSR2);
    let log = srv.log_file();
    let booted = poll(Duration::from_secs(20), || {
        (app_contexts(&log, "booted").len() >= 6).then_some(())
    });
    let tally = storm.halt();
    assert!(
        booted.is_some(),
        "every app booted again\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        (tally.failed, tally.refused, tally.dropped),
        (0, 0, 0),
        "failed, refused and dropped requests; last error: {:?}\n{}",
        tally.last_err,
        diagnostics(&srv),
    );
    assert!(tally.ok > 0, "the storm got no response");
    let mut after = worker_pids(srv.pid());
    after.sort();
    assert_eq!(
        after,
        pids,
        "the worker processes stay\n{}",
        diagnostics(&srv)
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

/// Classic mode runs every request in a new request cycle, so a reload has nothing to restart and the worker keeps serving.
#[test]
fn a_reload_leaves_a_classic_pool_alone() {
    let srv = spawn_with_config("classic/cwd.php", 1, "mode = \"classic\"\n");
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    signal(srv.pid(), libc::SIGUSR2);
    // Nothing must happen, so the test waits a fixed time before it looks.
    std::thread::sleep(Duration::from_secs(1));
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET /")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        worker_pids(srv.pid()),
        vec![pid],
        "the worker process stays\n{}",
        diagnostics(&srv)
    );
}

/// A reload into code that fails its boot makes the worker unhealthy. The pool booted before the reload, so the master replaces the worker and keeps the server running.
#[test]
fn a_reload_into_a_failing_boot_keeps_the_server_running() {
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/breakable-boot-worker.php"),
    )
    .json_log()
    .spawn();
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    wait_app_record(&srv.log_file(), "booted");
    std::fs::write(srv.dir.join("http/broken.flag"), "").expect("break the boot");
    signal(srv.pid(), libc::SIGUSR2);
    // A request during a failed boot gets 503 and starts the next boot at once, so a few requests make the worker unhealthy.
    for _ in 0..5 {
        let _ = http_get(srv.addr, "/", Duration::from_secs(10));
    }
    assert!(
        wait_log_contains(&srv, "flagged unhealthy", Duration::from_secs(20)),
        "the worker stays healthy\n{}",
        diagnostics(&srv)
    );
    let exit = srv.wait_exit(Duration::from_secs(3));
    assert_eq!(
        exit,
        None,
        "the master keeps running\n{}",
        diagnostics(&srv)
    );
    wait_workers(&srv, Duration::from_secs(20), "a replacement worker", |p| {
        p.len() == 1 && p[0] != pid
    });
}
