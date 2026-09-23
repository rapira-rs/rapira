use extension_api::{Extension, Php, Request, Result};
use php_sys::{Mode, Rapira};
use rapira_runtime::ExtensionRuntime;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tests::{Response, collect, fixture, php_lock};

/// Distinct ids so the same type can be registered many times (dup-name check).
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

async fn exec_full(php: &Php, req: Request) -> Result<Response> {
    collect(php.exec(req).await?).await
}

fn get_request(uri: &str) -> Request {
    Request {
        method: "GET".into(),
        uri: uri.into(),
        target: None,
        authority: None,
        https: false,
        protocol: "HTTP/1.1".into(),
        remote: extension_api::Addr::Inet(([127, 0, 0, 1], 44123).into()),
        server: extension_api::Addr::Inet(([127, 0, 0, 1], 80).into()),
        server_name: "localhost".into(),
        server_port: 80,
        tls: None,
        received_at: None,
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
        let (a, b) = tokio::join!(
            exec_full(&php, get_request("/?from=a")),
            exec_full(&php, get_request("/?from=b")),
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

/// A rejected body surfaces as a downcastable `Rejected` at exec() and never reaches the pool.
struct RejectDriver {
    id: String,
}

impl Extension for RejectDriver {
    type Config = ();

    fn init(_config: ()) -> Self {
        RejectDriver {
            id: format!("ext{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)),
        }
    }

    fn name(&self) -> &str {
        &self.id
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        let multipart_post = |body: Vec<u8>| {
            let mut r = get_request("/?from=a");
            r.method = "POST".into();
            r.headers = vec![(
                "content-type".into(),
                b"multipart/form-data; boundary=B".to_vec(),
            )];
            r.body = body;
            r
        };

        let err = match exec_full(&php, multipart_post(b"no boundary here".to_vec())).await {
            Err(e) => e,
            Ok(_) => anyhow::bail!("malformed multipart must reject"),
        };
        let rejected = err
            .downcast_ref::<extension_api::Rejected>()
            .ok_or_else(|| anyhow::anyhow!("expected Rejected, got {err:#}"))?;
        anyhow::ensure!(
            rejected.status == 400,
            "expected 400, got {}",
            rejected.status
        );

        let big = [
            b"--B\r\ncontent-disposition: form-data; name=f; filename=a\r\n\r\n".to_vec(),
            vec![b'x'; 8192],
            b"\r\n--B--".to_vec(),
        ]
        .concat();
        let err = match exec_full(&php, multipart_post(big)).await {
            Err(e) => e,
            Ok(_) => anyhow::bail!("over-limit file part must reject"),
        };
        let rejected = err
            .downcast_ref::<extension_api::Rejected>()
            .ok_or_else(|| anyhow::anyhow!("expected Rejected, got {err:#}"))?;
        anyhow::ensure!(
            rejected.status == 413,
            "expected 413, got {}",
            rejected.status
        );

        let plain_line = || ("content-type".to_string(), b"text/plain".to_vec());
        let multipart_line = || {
            (
                "content-type".to_string(),
                b"multipart/form-data; boundary=EVIL".to_vec(),
            )
        };
        for headers in [
            vec![plain_line(), multipart_line()],
            vec![multipart_line(), plain_line()],
        ] {
            let mut smuggle = multipart_post(
                b"--EVIL\r\ncontent-disposition: form-data; name=a\r\n\r\n1\r\n--EVIL--".to_vec(),
            );
            smuggle.headers = headers;
            let err = match exec_full(&php, smuggle).await {
                Err(e) => e,
                Ok(_) => anyhow::bail!("repeated content-type with a multipart body must reject"),
            };
            let rejected = err
                .downcast_ref::<extension_api::Rejected>()
                .ok_or_else(|| anyhow::anyhow!("expected Rejected, got {err:#}"))?;
            anyhow::ensure!(
                rejected.status == 400,
                "expected 400, got {}",
                rejected.status
            );
        }

        check(
            &exec_full(&php, multipart_post(Vec::new())).await?,
            "method=POST body=",
        )?;
        let mut plain = multipart_post(b"--B\r\nnot really\r\n--B--".to_vec());
        plain.headers = vec![("content-type".into(), b"text/plain".to_vec())];
        check(
            &exec_full(&php, plain).await?,
            "method=POST body=--B\r\nnot really\r\n--B--",
        )?;

        check(
            &exec_full(&php, get_request("/?from=a")).await?,
            "method=GET body=",
        )
    }
}

#[test]
fn rejected_bodies_never_reach_the_pool() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::Dispatcher(fixture("dispatcher/echo-loop-worker.php")))?;
    let mut host = ExtensionRuntime::new();
    host.register::<RejectDriver>(())?;
    let limits = rapira_runtime::multipart::Limits {
        max_file_size: 1024,
        ..rapira_runtime::multipart::Limits::default()
    };
    let outcomes = host
        .run_with_options(
            rapira.handle(),
            fixture("dispatcher/echo-loop-worker.php"),
            rapira_runtime::RuntimeOptions {
                uploads: std::sync::Arc::new(limits),
                ..rapira_runtime::RuntimeOptions::default()
            },
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
    let rapira = Rapira::start(Mode::Classic)?;
    let mut host = ExtensionRuntime::new();
    host.register::<Driver>(())?;
    let outcomes = host
        .run(
            rapira.handle(),
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

/// A buffered head-only error response must not be flagged truncated, or exec serves a generic 502 instead of the real 404.
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
        let resp = exec_full(&php, get_request("/")).await?;
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
fn exec_delivers_buffered_error_response_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::Worker(fixture(
        "shared/error-keeps-headers-worker.php",
    )))?;
    let mut host = ExtensionRuntime::new();
    host.register::<ErrorPathDriver>(())?;
    let outcomes = host
        .run(
            rapira.handle(),
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

/// Output before the throw seals a truncated frame: `exec` must error instead of delivering a possibly-incomplete body.
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
        let err = match exec_full(&php, get_request("/")).await {
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
fn exec_rejects_truncated_response_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::Worker(fixture("shared/output-then-throw-worker.php")))?;
    let mut host = ExtensionRuntime::new();
    host.register::<TruncatedDriver>(())?;
    let outcomes = host
        .run(
            rapira.handle(),
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
        .run(rapira.handle(), fixture("shared/error-keeps-headers.php"))
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

/// A long-lived extension whose `run` never returns on its own: it runs until the host signals shutdown.
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
        rapira.handle(),
        fixture("extension_tests/ext-driver-classic.php"),
    );

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
fn many_extensions_run() -> anyhow::Result<()> {
    let _guard = php_lock();
    const N: usize = 12;
    let rapira = Rapira::start(Mode::Worker(fixture(
        "extension_tests/ext-driver-worker.php",
    )))?;
    let mut host = ExtensionRuntime::new();
    for _ in 0..N {
        host.register::<Driver>(())?;
    }
    let outcomes = host
        .run(
            rapira.handle(),
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

/// A fixed name so two registrations collide.
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

fn run_one<E: Extension<Config = ()>>() -> anyhow::Result<Vec<Result<(), String>>> {
    let _guard = php_lock();
    let rapira = Rapira::start(Mode::Classic)?;
    let mut host = ExtensionRuntime::new();
    host.register::<E>(())?;
    let outcomes = host
        .run(
            rapira.handle(),
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
    let running = host.run_with_options(
        rapira.handle(),
        fixture("extension_tests/ext-driver-classic.php"),
        rapira_runtime::RuntimeOptions {
            grace: Duration::from_millis(100),
            ..rapira_runtime::RuntimeOptions::default()
        },
    );
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
