use php_sys::{Mode, Rapira};
use std::sync::mpsc;
use std::time::Duration;
use tests::{drain, fixture, php_lock, req};

// A worker that fatals before its receive loop must 503 the queued job and let Drop return instead of joining a forever-retrying boot.
#[test]
fn failboot_worker_serves_503_and_drops_cleanly() -> anyhow::Result<()> {
    let _guard = php_lock();
    let (done_tx, done_rx) = mpsc::sync_channel::<(u16, String)>(1);

    let scenario = std::thread::spawn(move || -> anyhow::Result<()> {
        let r = Rapira::start(Mode::Dispatcher(fixture(
            "failboot_worker_tests/failboot-worker.php",
        )))?;
        let h = r.handle();
        let rx = h.handle_blocking(req("/", "failboot_worker_tests/failboot-worker.php"))?;
        drop(h);
        let (status, body) = drain(rx);
        drop(r);
        let _ = done_tx.send((status, body));
        Ok(())
    });

    let (status, _body) = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("broken worker black-holed the request or hung Drop (A6 regression)");
    assert_eq!(status, 503, "a boot-failed worker must 503 the queued job");
    scenario.join().expect("scenario thread panicked")?;
    Ok(())
}

// UNHEALTHY_AFTER (5) consecutive boot failures must set the scoreboard unhealthy flag, each failed boot 503ing its queued job.
#[test]
fn failboot_worker_flags_unhealthy_after_threshold() -> anyhow::Result<()> {
    let _guard = php_lock();
    let (done_tx, done_rx) = mpsc::sync_channel::<(bool, Vec<u16>)>(1);

    let scenario = std::thread::spawn(move || -> anyhow::Result<()> {
        let r = Rapira::start(Mode::Dispatcher(fixture(
            "failboot_worker_tests/failboot-worker.php",
        )))?;
        let h = r.handle();
        let mut statuses = Vec::new();
        for _ in 0..5 {
            let (s, _) =
                drain(h.handle_blocking(req("/", "failboot_worker_tests/failboot-worker.php"))?);
            statuses.push(s);
        }
        let unhealthy = r.scoreboard().expect("private scoreboard slot").unhealthy;
        drop(h);
        r.shutdown();
        let _ = done_tx.send((unhealthy, statuses));
        Ok(())
    });

    let (unhealthy, statuses) = done_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("boot-failing worker hung (unhealthy/Drop regression)");
    assert!(
        statuses.iter().all(|&s| s == 503),
        "each boot-failed job must 503 (got {statuses:?})"
    );
    assert!(
        unhealthy,
        "5 consecutive boot failures must flag the worker unhealthy"
    );
    scenario.join().expect("scenario thread panicked")?;
    Ok(())
}
