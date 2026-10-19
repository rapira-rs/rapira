#![cfg(not(target_os = "macos"))]

use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rapira_sapi::plugin::Mode;
use tests::wire::submit;
use tests::{drain, drain_resp_deadline, fixture, req, server_log};

use crate::harness::{Spawn, http_get, scratch_dir, slot_line, worker_pids};

fn timeout_ini() -> String {
    std::fs::read_to_string(fixture("ini/timeout_tests/timeout.php.ini"))
        .expect("read timeout.php.ini")
}

/// Pins that receive() disarms the execution timer while parked: a worker parked past the 1s budget still serves instead of fataling and being 503-shed.
#[test]
fn parked_receive_outlives_the_execution_budget() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/echo-loop-worker.php"))
        .php_ini(&timeout_ini())
        .spawn();

    let (status, body) = drain(submit(srv.addr, req("/warmup"))?);
    assert_eq!((status, body.as_str()), (200, "method=GET body="));

    std::thread::sleep(Duration::from_secs(2));
    let (status, body) = drain(submit(srv.addr, req("/first"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "method=GET body="),
        "a worker parked past the budget must still serve"
    );
    Ok(())
}

/// Pins that the budget re-armed at unit handout still fires: a spinning unit is killed before its response head, the plugin answers it 502, and the recycled worker keeps serving.
#[test]
fn rearmed_budget_kills_a_spinning_unit() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php"))
        .php_ini(&timeout_ini())
        .spawn();

    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!((status, body.as_str()), (200, "state=false"));

    let mut rx = submit(srv.addr, req("/?probe=spin"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let resp = drain_resp_deadline(&mut rx, deadline)
        .expect("spinning unit was never killed - the per-unit budget did not re-arm");
    assert!(
        resp.status() == 502 && resp.ended,
        "a killed unit is answered 502 (got status {})",
        resp.status()
    );

    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must recover after the timeout"
    );
    Ok(())
}

/// Pins that the per-unit re-arm with reset_signals=0 still delivers SIGPROF: a spin on a later unit in the same cycle is killed, not left running.
#[test]
fn max_execution_time_fires_on_rearmed_units() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("timeout_tests/timeout-worker.php"))
        .php_ini(&timeout_ini())
        .spawn();

    let (status, body) = drain(submit(srv.addr, req("/timeout-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "ok"));

    let mut rx = submit(srv.addr, req("/timeout-worker.php?mode=spin"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let resp = drain_resp_deadline(&mut rx, deadline)
        .expect("spinning unit was never killed - max_execution_time did not fire");
    assert!(resp.ended, "worker died without sealing a response");
    let body = resp.body_string();
    assert!(
        body.contains("Maximum execution time"),
        "the timeout fatal must reach the body (got: {body:?})"
    );
    assert_eq!(resp.status(), 200);

    let (status, body) = drain(submit(srv.addr, req("/timeout-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "ok"),
        "the worker must recover after a timeout"
    );
    Ok(())
}

/// Pins that a unit that waits on a socket keeps its worker under static-file load. The execution timer counts the CPU time of all threads of the process, so the IO threads use up the budget during the wait. The hard timeout must not end the worker process.
#[test]
fn socket_wait_under_static_load_keeps_the_worker() -> anyhow::Result<()> {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let port = backend.local_addr()?.port();
    let root = scratch_dir();
    std::fs::write(root.join("s.txt"), "static")?;
    let srv = Spawn::http(
        Mode::Worker,
        fixture("timeout_tests/socket-wait-worker.php"),
    )
    .php_ini(&timeout_ini())
    .http_extra(&format!(
        "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
        root.display()
    ))
    .spawn();
    let workers = worker_pids(srv.pid());

    // The backend answers after the 1 s budget plus the 2 s hard timeout of PHP.
    let answer = std::thread::spawn(move || -> std::io::Result<()> {
        let (mut conn, _) = backend.accept()?;
        std::thread::sleep(Duration::from_secs(5));
        conn.write_all(b"done\n")
    });
    let stop = Arc::new(AtomicBool::new(false));
    let load: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            let addr = srv.addr;
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = http_get(addr, "/s.txt", Duration::from_secs(2));
                }
            })
        })
        .collect();

    let mut rx = submit(srv.addr, req(&format!("/?port={port}")))?;
    let resp = drain_resp_deadline(&mut rx, Instant::now() + Duration::from_secs(15));
    stop.store(true, Ordering::Relaxed);
    for t in load {
        t.join().expect("load thread");
    }
    answer.join().expect("backend thread")?;

    let resp = resp.expect("the waiting unit never ended");
    assert!(
        resp.status() == 200 && resp.ended,
        "the waiting unit must get a full response (got status {})",
        resp.status()
    );
    let body = resp.body_string();
    assert!(
        body.contains("Maximum execution time"),
        "the static load must use up the budget during the wait (got: {body:?})"
    );
    assert_eq!(
        worker_pids(srv.pid()),
        workers,
        "the timeout must not end the worker process"
    );
    Ok(())
}

/// Pins the budget of a unit that is already queued when receive() is called: the spin is killed with the timeout fatal, a unit that fits the budget completes after a unit that used most of its own, and a 0 budget stays untimed after set_time_limit().
#[test]
fn queued_unit_gets_a_fresh_budget() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        first: &'static str,
        queued: &'static str,
        killed: bool,
    }
    let cases = [
        Case {
            name: "queued spin is killed",
            fixture: "timeout_tests/timeout-dispatcher.php",
            first: "/?burn=300",
            queued: "/?spin",
            killed: true,
        },
        Case {
            name: "queued unit gets a full budget",
            fixture: "timeout_tests/timeout-dispatcher.php",
            first: "/?burn=600",
            queued: "/?burn=600",
            killed: false,
        },
        Case {
            name: "queued unit runs untimed after set_time_limit() on a 0 budget",
            fixture: "timeout_tests/timeout-dispatcher-unlimited.php",
            first: "/?limit=1&burn=500",
            queued: "/?burn=1000",
            killed: false,
        },
    ];

    let ini = timeout_ini();
    for c in cases {
        let srv = Spawn::http(Mode::Dispatcher, fixture(c.fixture))
            .php_ini(&ini)
            .json_log()
            .spawn();

        let first = submit(srv.addr, req(c.first))?;
        // Two connections reach the worker in any order, so the second request goes out once the first unit runs: slot state 3 is SLOT_ACTIVE.
        slot_line(&srv, "state 3 ");
        let mut queued = submit(srv.addr, req(c.queued))?;
        assert_eq!(
            drain(first),
            (200, "pending=1".to_owned()),
            "{}: the second unit must be queued before the next receive()",
            c.name
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        let resp = drain_resp_deadline(&mut queued, deadline)
            .unwrap_or_else(|| panic!("{}: the queued unit never ended", c.name));
        let fatal = server_log::records(&srv.log_file())
            .iter()
            .any(|e| e.target == "php" && e.message.contains("Maximum execution time"));
        if c.killed {
            assert!(
                resp.status() == 502 && resp.ended,
                "{}: a killed unit is answered 502 (got status {})",
                c.name,
                resp.status()
            );
            assert!(fatal, "{}: the timeout fatal must be logged", c.name);
        } else {
            assert_eq!(
                (resp.status(), resp.body_string().as_str()),
                (200, "pending=0"),
                "{}",
                c.name
            );
            assert!(!fatal, "{}: no timeout fatal expected", c.name);
        }

        let (status, body) = drain(submit(srv.addr, req("/"))?);
        assert_eq!(
            (status, body.as_str()),
            (200, "pending=0"),
            "{}: the worker must keep serving",
            c.name
        );
    }
    Ok(())
}
