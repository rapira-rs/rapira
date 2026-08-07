use php_sys::{Mode, Rapira};
use std::sync::mpsc;
use std::time::Duration;
use tests::{drain, fixture, php_lock, req};

// A6: a worker script that fatals before its `handleRequest` loop can
// never read the intake channel from PHP. The Rust boot-failure drain must
// (a) answer the queued job with 503 and (b) observe channel closure so
// `Drop for Rapira` returns instead of joining a worker that retries the boot
// forever. Pre-fix (retry-forever loop) BOTH the response and Drop hang.
#[test]
fn failboot_worker_serves_503_and_drops_cleanly() -> anyhow::Result<()> {
    let _guard = php_lock();
    let (done_tx, done_rx) = mpsc::sync_channel::<(u16, String)>(1);

    // Rapira is !Send: build, use, and drop it entirely on one thread. The test
    // thread only enforces a deadline so a regression fails loudly instead of
    // hanging the whole suite.
    let scenario = std::thread::spawn(move || -> anyhow::Result<()> {
        let r = Rapira::start(Mode::WorkerRequest(fixture(
            "failboot_worker_tests/failboot-worker.php",
        )))?;
        let h = r.handle()?;
        let rx = h.handle_blocking(req("/", "failboot_worker_tests/failboot-worker.php"))?;
        drop(h); // last non-Rapira intake sender — lets the channel close on drop(r)
        let (status, body) = drain(rx); // pre-fix: blocks forever (no 503 ever sent)
        drop(r); // pre-fix: joins a worker stuck in the retry loop -> hangs
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

// UNHEALTHY_AFTER (=5) consecutive boot failures must flag the worker unhealthy.
// Each failed boot answers one queued job with 503, then retries the boot; the 5th
// boot sets the scoreboard flag. Deadline-guarded so a regression (no 503 / hung
// Drop) fails loudly instead of hanging the suite.
#[test]
fn failboot_worker_flags_unhealthy_after_threshold() -> anyhow::Result<()> {
    let _guard = php_lock();
    let (done_tx, done_rx) = mpsc::sync_channel::<(usize, Vec<u16>)>(1);

    let scenario = std::thread::spawn(move || -> anyhow::Result<()> {
        let r = Rapira::start(Mode::WorkerRequest(fixture(
            "failboot_worker_tests/failboot-worker.php",
        )))?;
        let h = r.handle()?;
        let mut statuses = Vec::new();
        for _ in 0..5 {
            let (s, _) =
                drain(h.handle_blocking(req("/", "failboot_worker_tests/failboot-worker.php"))?);
            statuses.push(s);
        }
        let unhealthy = r.scoreboard().unhealthy;
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
    assert_eq!(
        unhealthy, 1,
        "5 consecutive boot failures must flag the worker unhealthy"
    );
    scenario.join().expect("scenario thread panicked")?;
    Ok(())
}
