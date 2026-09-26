#![cfg(not(target_os = "macos"))]

use std::time::{Duration, Instant};

use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{drain, drain_resp_deadline, fixture, req, server_log};

use crate::harness::{Server, Spawn, signal};

fn timeout_ini() -> String {
    std::fs::read_to_string(fixture("ini/timeout_tests/timeout.php.ini"))
        .expect("read timeout.php.ini")
}

/// Sends SIGUSR1 until the master logs a scoreboard line of slot 0 that contains `fragment`, for at most 10 s; returns that line.
fn slot_line(srv: &Server, fragment: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        signal(srv.pid(), libc::SIGUSR1);
        std::thread::sleep(Duration::from_millis(20));
        let log = std::fs::read_to_string(srv.log_file()).unwrap_or_default();
        if let Some(line) = log
            .lines()
            .find(|l| l.contains("slot 0 pid") && l.contains(fragment))
        {
            return line.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "no slot 0 line with {fragment:?} within 10s\n{log}"
        );
    }
}

/// Pins that receive() disarms the wall timer while parked: a worker parked past the 1s budget still serves instead of fataling and being 503-shed.
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

/// Pins that the per-job re-arm with reset_signals=0 still delivers SIGRTMIN: a spin on a later job in the same cycle is killed, not left running.
#[test]
fn max_execution_time_fires_on_rearmed_jobs() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("timeout_tests/timeout-worker.php"))
        .php_ini(&timeout_ini())
        .spawn();

    let (status, body) = drain(submit(srv.addr, req("/timeout-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "ok"));

    let mut rx = submit(srv.addr, req("/timeout-worker.php?mode=spin"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let resp = drain_resp_deadline(&mut rx, deadline)
        .expect("spinning job was never killed - max_execution_time did not fire");
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
