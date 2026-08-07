//! End-to-end: a native extension drives requests through PHP via `Php`.

use extension_api::{Extension, Php, Request, Response, Result};
use php_sys::{Mode, Rapira};
use rapira_runtime::ExtensionRuntime;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tests::{fixture, php_lock};

/// Distinct ids so the same type can be registered many times (dup-name check).
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// A bodyless `GET` for `uri` with the defaults every driver here needs.
fn get_request(uri: &str) -> Request {
    Request {
        method: "GET".into(),
        uri: uri.into(),
        https: false,
        protocol: "HTTP/1.1".into(),
        remote_addr: "127.0.0.1".into(),
        remote_port: 0,
        server_name: "localhost".into(),
        server_port: 80,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

/// Drives two requests concurrently; distinct bodies prove both ran.
struct Driver {
    id: String,
}

impl Extension for Driver {
    type Config = ();

    fn init(_config: ()) -> Self {
        Driver {
            id: format!("ext{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)),
        }
    }

    fn name(&self) -> &str {
        &self.id
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        // `join!` starts both exec subtasks before awaiting either, so both are in flight
        // through the PHP pool concurrently (both must complete; not a strict parallelism proof).
        let (a, b) = tokio::join!(
            php.exec(get_request("/?from=a")),
            php.exec(get_request("/?from=b")),
        );
        check(&a?, "ok:a")?;
        check(&b?, "ok:b")?;
        Ok(())
    }
}

fn check(res: &Response, want: &str) -> Result<()> {
    anyhow::ensure!(res.status == 200, "expected 200, got {}", res.status);
    anyhow::ensure!(
        res.body == want.as_bytes(),
        "expected body {want:?}, got {:?}",
        String::from_utf8_lossy(&res.body)
    );
    Ok(())
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn an_extension_drives_concurrent_requests_through_php() -> anyhow::Result<()> {
    let _guard = php_lock();
    // Worker mode: the resident script answers each exec with "ok:<from>". The two
    // join!ed execs serialize onto the single interpreter; this proves completion,
    // not parallelism.
    let rapira = Rapira::start(Mode::WorkerRequest(fixture(
        "extension_tests/ext-driver-worker.php",
    )))?;
    let mut host = ExtensionRuntime::new();
    host.register::<Driver>(())?;
    let outcomes = host
        .run(
            rapira.handle()?,
            fixture("extension_tests/ext-driver-worker.php"),
        )
        .join();
    drop(rapira);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].is_ok(), "driver failed: {:?}", outcomes[0]);
    Ok(())
}

#[test]
fn classic_mode_serves_exec() -> anyhow::Result<()> {
    let _guard = php_lock();
    // Classic mode runs the front controller per exec, with the URI in $_GET, so it
    // echoes "ok:<from>" — exec works with a real front controller (why serve takes a SCRIPT).
    let rapira = Rapira::start(Mode::Classic)?;
    let mut host = ExtensionRuntime::new();
    host.register::<Driver>(())?;
    let outcomes = host
        .run(
            rapira.handle()?,
            fixture("extension_tests/ext-driver-classic.php"),
        )
        .join();
    drop(rapira);
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].is_ok(),
        "classic exec failed: {:?}",
        outcomes[0]
    );
    Ok(())
}

/// Drives one request whose PHP handler sets a status + session cookie and then throws
/// with output buffered — a COMPLETE, head-only error response. Regression guard for the
/// truncation rule (`Context::is_truncated`): `exec` maps a truncated terminal frame to
/// an error, so a buffered/head-only error response must NOT be flagged truncated, or the
/// extension would serve a generic 502 instead of the real 404.
struct ErrorPathDriver;

impl Extension for ErrorPathDriver {
    type Config = ();

    fn init(_config: ()) -> Self {
        ErrorPathDriver
    }

    fn name(&self) -> &str {
        "error-path-driver"
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        // Before the fix this returned Err("php crashed mid-response; body truncated").
        let resp = php.exec(get_request("/")).await?;
        anyhow::ensure!(resp.status == 404, "expected 404, got {}", resp.status);
        anyhow::ensure!(
            resp.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("set-cookie")),
            "the session Set-Cookie must survive the buffered error path"
        );
        Ok(())
    }
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn exec_delivers_buffered_error_response_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::WorkerRequest(fixture(
        "shared/error-keeps-headers-worker.php",
    )))?;
    let mut host = ExtensionRuntime::new();
    host.register::<ErrorPathDriver>(())?;
    let outcomes = host
        .run(
            rapira.handle()?,
            fixture("shared/error-keeps-headers-worker.php"),
        )
        .join();
    drop(rapira);
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].is_ok(),
        "exec rejected a complete buffered error response: {:?}",
        outcomes[0]
    );
    Ok(())
}

/// Drives one request whose handler echoes and THEN throws — body output began
/// during the handler, so the sealed frame is truncated and `exec` must surface
/// it as an error rather than deliver a possibly-incomplete body.
struct TruncatedDriver;

impl Extension for TruncatedDriver {
    type Config = ();

    fn init(_config: ()) -> Self {
        TruncatedDriver
    }

    fn name(&self) -> &str {
        "truncated-driver"
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        let err = match php.exec(get_request("/")).await {
            Ok(resp) => anyhow::bail!(
                "exec must reject a truncated response, got {} with body {:?}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            ),
            Err(e) => e,
        };
        anyhow::ensure!(
            err.to_string().contains("truncated"),
            "expected the truncated-response error, got: {err:#}"
        );
        Ok(())
    }
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn exec_rejects_truncated_response_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::WorkerRequest(fixture(
        "shared/output-then-throw-worker.php",
    )))?;
    let mut host = ExtensionRuntime::new();
    host.register::<TruncatedDriver>(())?;
    let outcomes = host
        .run(
            rapira.handle()?,
            fixture("shared/output-then-throw-worker.php"),
        )
        .join();
    drop(rapira);
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].is_ok(),
        "exec must map a truncated frame to an error: {:?}",
        outcomes[0]
    );
    Ok(())
}

#[test]
fn exec_delivers_buffered_error_response_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::Classic)?;
    let mut host = ExtensionRuntime::new();
    host.register::<ErrorPathDriver>(())?;
    let outcomes = host
        .run(rapira.handle()?, fixture("shared/error-keeps-headers.php"))
        .join();
    drop(rapira);
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].is_ok(),
        "classic exec rejected a complete buffered error response: {:?}",
        outcomes[0]
    );
    Ok(())
}

/// A long-lived server extension whose `run` never returns on its own — it runs until the
/// host signals shutdown.
struct Resident;

static RESIDENT_SHUTDOWN: AtomicBool = AtomicBool::new(false);

impl Extension for Resident {
    type Config = ();

    fn init(_config: ()) -> Self {
        Resident
    }

    fn name(&self) -> &str {
        "resident"
    }

    async fn run(&mut self, _php: Php) -> Result<()> {
        std::future::pending().await
    }

    async fn shutdown(&mut self) -> Result<()> {
        RESIDENT_SHUTDOWN.store(true, Ordering::Relaxed);
        Ok(())
    }
}

#[test]
fn teardown_cancels_run_and_drives_shutdown() -> anyhow::Result<()> {
    let _guard = php_lock();
    RESIDENT_SHUTDOWN.store(false, Ordering::Relaxed);
    let rapira = Rapira::start(Mode::Classic)?;
    let mut host = ExtensionRuntime::new();
    host.register::<Resident>(())?;
    let running = host.run(
        rapira.handle()?,
        fixture("extension_tests/ext-driver-classic.php"),
    );

    // Dropping the guard fires the internal stop: `run` (which never returns) is
    // cancelled, `shutdown` is driven, and the tasks drain — promptly, not hanging.
    let start = Instant::now();
    drop(running);
    drop(rapira);
    assert!(
        RESIDENT_SHUTDOWN.load(Ordering::Relaxed),
        "shutdown must be driven when a resident run is cancelled"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "graceful stop must not hang"
    );
    Ok(())
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn many_extensions_run() -> anyhow::Result<()> {
    let _guard = php_lock();
    const N: usize = 12;
    // The fan-out (12 drivers × 2 execs) serializes onto the single PHP interpreter.
    // This proves all N extensions complete, not a strict parallelism bound.
    let rapira = Rapira::start(Mode::WorkerRequest(fixture(
        "extension_tests/ext-driver-worker.php",
    )))?;
    let mut host = ExtensionRuntime::new();
    for _ in 0..N {
        host.register::<Driver>(())?;
    }
    let outcomes = host
        .run(
            rapira.handle()?,
            fixture("extension_tests/ext-driver-worker.php"),
        )
        .join();
    drop(rapira);
    assert_eq!(outcomes.len(), N);
    assert!(
        outcomes.iter().all(|r| r.is_ok()),
        "some extensions failed: {outcomes:?}"
    );
    Ok(())
}

/// A fixed name (unlike `Driver`, whose id is unique per instance) so two registrations collide.
struct Fixed;

impl Extension for Fixed {
    type Config = ();

    fn init(_config: ()) -> Self {
        Fixed
    }

    fn name(&self) -> &str {
        "fixed"
    }
    async fn run(&mut self, _php: Php) -> Result<()> {
        Ok(())
    }
}

#[test]
fn duplicate_extension_name_is_rejected() {
    let mut host = ExtensionRuntime::new();
    host.register::<Fixed>(()).unwrap();
    let err = host.register::<Fixed>(()).unwrap_err();
    assert!(
        err.to_string().contains("duplicate extension"),
        "expected a duplicate-name error, got: {err}"
    );
}

/// Register one `E`, drive it to completion in classic mode, and return its outcomes.
fn run_one<E: Extension<Config = ()>>() -> anyhow::Result<Vec<Result<(), String>>> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::Classic)?;
    let mut host = ExtensionRuntime::new();
    host.register::<E>(())?;
    let outcomes = host
        .run(
            rapira.handle()?,
            fixture("extension_tests/ext-driver-classic.php"),
        )
        .join();
    drop(rapira);
    Ok(outcomes)
}

/// `run` fails; the host must surface the error as this extension's outcome.
struct Failing;

impl Extension for Failing {
    type Config = ();

    fn init(_config: ()) -> Self {
        Failing
    }

    fn name(&self) -> &str {
        "failing"
    }
    async fn run(&mut self, _php: Php) -> Result<()> {
        anyhow::bail!("boom")
    }
}

#[test]
fn run_returning_err_is_reported() -> anyhow::Result<()> {
    let outcomes = run_one::<Failing>()?;
    assert_eq!(outcomes.len(), 1);
    let err = outcomes[0].as_ref().unwrap_err();
    assert!(
        err.contains("run failed"),
        "expected a run failure, got: {err}"
    );
    Ok(())
}

/// `run` panics; the host must convert the JoinError into an outcome, not abort.
struct Panicking;

impl Extension for Panicking {
    type Config = ();

    fn init(_config: ()) -> Self {
        Panicking
    }

    fn name(&self) -> &str {
        "panicking"
    }
    async fn run(&mut self, _php: Php) -> Result<()> {
        panic!("kaboom")
    }
}

#[test]
fn panic_in_run_is_reported() -> anyhow::Result<()> {
    let outcomes = run_one::<Panicking>()?;
    assert_eq!(outcomes.len(), 1);
    let err = outcomes[0].as_ref().unwrap_err();
    assert!(
        err.contains("driver task panicked"),
        "expected a panic outcome, got: {err}"
    );
    Ok(())
}

/// `run` never returns and `shutdown` overruns the grace; the host must time it out.
struct SlowShutdown;

impl Extension for SlowShutdown {
    type Config = ();

    fn init(_config: ()) -> Self {
        SlowShutdown
    }

    fn name(&self) -> &str {
        "slow-shutdown"
    }
    async fn run(&mut self, _php: Php) -> Result<()> {
        std::future::pending().await
    }
    async fn shutdown(&mut self) -> Result<()> {
        // Overruns any sane grace; the host's timeout must fire first.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(())
    }
}

#[test]
fn shutdown_timeout_is_reported() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::Classic)?;
    let mut host = ExtensionRuntime::new();
    host.register::<SlowShutdown>(())?;
    // A tiny grace so the timeout branch fires fast instead of after the 30s default.
    let running = host.run_with_grace(
        rapira.handle()?,
        fixture("extension_tests/ext-driver-classic.php"),
        Duration::from_millis(100),
    );
    // `stop` cancels the pending `run`, then drives `shutdown` — which overruns the grace.
    let start = Instant::now();
    let outcomes = running.stop();
    drop(rapira);
    assert_eq!(outcomes.len(), 1);
    let err = outcomes[0].as_ref().unwrap_err();
    assert!(
        err.contains("shutdown timed out"),
        "expected a shutdown timeout, got: {err}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the timeout must be bounded by the grace, not hang"
    );
    Ok(())
}
