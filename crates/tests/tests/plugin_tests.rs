use http::header::{HeaderMap, SET_COOKIE};
use rapira_grpc::{Call, RpcProtocol, RpcStatus, UnaryCall};
use rapira_http::Exchange;
use rapira_net::PrepareCtx;
use rapira_sapi::plugin::{Mode, PhpPart, Plugin, Worker, run_plugin};
use rapira_sapi::work::{Intake, Work};
use rapira_sapi::{Addr, Rapira, Request};
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tests::{Fields, Response, collect, fields, fixture, php_lock, req};

const GRACE: Duration = Duration::from_secs(30);
/// The test plugins do not drain.
const DRAIN_GRACE: Duration = Duration::ZERO;

/// A plugin whose serve is `body`.
struct TestPlugin(Box<dyn FnOnce(Worker) -> anyhow::Result<()> + Send>);

impl TestPlugin {
    fn boxed(body: impl FnOnce(Worker) -> anyhow::Result<()> + Send + 'static) -> Box<Self> {
        Box::new(Self(Box::new(body)))
    }
}

impl Plugin for TestPlugin {
    fn name(&self) -> &'static str {
        "test"
    }

    fn modes(&self) -> &'static [Mode] {
        &[Mode::Classic, Mode::Worker, Mode::Dispatcher]
    }

    fn php(&self) -> Option<PhpPart> {
        None
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx) -> anyhow::Result<()> {
        Ok(())
    }

    fn serve(self: Box<Self>, worker: Worker) -> anyhow::Result<()> {
        (self.0)(worker)
    }
}

/// A plugin that runs `drive` on the plugin runtime against an intake on the worker's sink, and returns its outcome.
fn driver<U, F>(drive: impl FnOnce(Intake<U>) -> F + Send + 'static) -> Box<TestPlugin>
where
    U: Work,
    F: Future<Output = anyhow::Result<()>>,
{
    TestPlugin::boxed(move |worker| {
        worker
            .handle
            .block_on(drive(Intake::new(worker.sink.clone())))
    })
}

/// Runs `plugin` on the worker of `rapira` until it returns.
fn run_to_end(
    plugin: Box<TestPlugin>,
    rapira: &Rapira,
    entrypoint: PathBuf,
    mode: Mode,
) -> anyhow::Result<()> {
    run_plugin(plugin, rapira.sink(), GRACE, DRAIN_GRACE, entrypoint, mode)?.join()
}

async fn exchange(intake: &Intake<Exchange>, req: Request) -> anyhow::Result<Response> {
    let (exchange, reply) = Exchange::new(req);
    intake.submit(exchange).await?;
    collect(reply).await
}

fn check(res: &Response, want: &str) -> anyhow::Result<()> {
    anyhow::ensure!(res.status == 200, "expected 200, got {}", res.status);
    anyhow::ensure!(
        res.body == want.as_bytes(),
        "expected body {want:?}, got {:?}",
        String::from_utf8_lossy(&res.body)
    );
    Ok(())
}

/// Submits two exchanges concurrently; distinct bodies prove both ran.
fn two_exchanges() -> Box<TestPlugin> {
    driver(|intake: Intake<Exchange>| async move {
        let (a, b) = tokio::join!(
            exchange(&intake, req("/?from=a")),
            exchange(&intake, req("/?from=b")),
        );
        check(&a?, "ok:a")?;
        check(&b?, "ok:b")
    })
}

/// A unary reply as headers, trailers, and the output message or the status. None: the call was lost.
type Parts = Option<(
    HeaderMap,
    HeaderMap,
    std::result::Result<Vec<u8>, RpcStatus>,
)>;

#[derive(Debug)]
enum Want {
    /// The response headers, the trailers and the output message.
    Reply(Fields, Fields, &'static str),
    /// The status triple, with no response metadata.
    Status(u32, &'static str, &'static [(&'static str, &'static [u8])]),
    Lost,
}

impl Want {
    fn parts(&self) -> Parts {
        match self {
            Self::Reply(headers, trailers, message) => Some((
                fields(headers),
                fields(trailers),
                Ok(message.as_bytes().to_vec()),
            )),
            Self::Status(code, message, details) => Some((
                HeaderMap::new(),
                HeaderMap::new(),
                Err(RpcStatus {
                    code: *code,
                    message: (*message).to_owned(),
                    details: details
                        .iter()
                        .map(|&(url, value)| (url.to_owned(), value.into()))
                        .collect(),
                }),
            )),
            Self::Lost => None,
        }
    }
}

struct UnaryCase {
    name: &'static str,
    message: &'static str,
    expected: Want,
}

// One row per outcome of a call: a reply with both halves, a status, a lost call. grpc_dispatcher.rs fixes the values.
const UNARY_CASES: &[UnaryCase] = &[
    UnaryCase {
        name: "status",
        message: "fail",
        expected: Want::Status(
            5,
            "no invoice",
            &[("type.googleapis.com/google.rpc.ErrorInfo", b"\x0a\x01x")],
        ),
    },
    UnaryCase {
        name: "metadata",
        message: "meta",
        expected: Want::Reply(&[("x-h", "v"), ("x-b-bin", "AQI")], &[("x-t", "w")], "meta"),
    },
    UnaryCase {
        name: "lost",
        message: "drop",
        expected: Want::Lost,
    },
];

#[test]
fn unary_calls_cross_the_intake() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = fixture("grpc/unary-worker.php");
    let rapira = tests::start_grpc(script.clone())?;
    let plugin = driver(|intake: Intake<Call>| async move {
        let mut mismatches = Vec::new();
        for c in UNARY_CASES {
            let (call, reply) = Call::new(UnaryCall {
                method: "rapira.test.v1.EchoService/Echo".into(),
                protocol: RpcProtocol::Connect,
                metadata: HeaderMap::new(),
                deadline: None,
                remote: Addr::Inet(([127, 0, 0, 1], 44123).into()),
                message: c.message.as_bytes().to_vec().into(),
            });
            intake.submit(call).await?;
            let got: Parts = reply
                .await
                .ok()
                .map(|r| (r.headers, r.trailers, r.outcome.map(|m| m.to_vec())));
            let expected = c.expected.parts();
            if got != expected {
                mismatches.push(format!("{}: expected {expected:?}, got {got:?}", c.name));
            }
        }
        anyhow::ensure!(mismatches.is_empty(), "{mismatches:#?}");
        Ok(())
    });
    let outcome = run_to_end(plugin, &rapira, script, Mode::Dispatcher);
    drop(rapira);
    outcome
}

#[test]
fn classic_mode_serves_exchanges() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = fixture("plugin_tests/driver-classic.php");
    let rapira = Rapira::start(&tests::PHP_PARTS, Mode::Classic, script.clone(), None)?;
    let outcome = run_to_end(two_exchanges(), &rapira, script, Mode::Classic);
    drop(rapira);
    outcome
}

/// A buffered head-only error response must not be flagged truncated, or the plugin serves a generic 502 instead of the real 404.
fn error_path_driver() -> Box<TestPlugin> {
    driver(|intake: Intake<Exchange>| async move {
        let resp = exchange(&intake, req("/")).await?;
        anyhow::ensure!(resp.status == 404, "expected 404, got {}", resp.status);
        anyhow::ensure!(
            resp.headers.contains_key(SET_COOKIE),
            "the session Set-Cookie must survive the buffered error path"
        );
        Ok(())
    })
}

#[test]
fn buffered_error_response_arrives_whole_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = fixture("shared/error-keeps-headers-worker.php");
    let rapira = Rapira::start(&tests::PHP_PARTS, Mode::Worker, script.clone(), None)?;
    let outcome = run_to_end(error_path_driver(), &rapira, script, Mode::Worker);
    drop(rapira);
    outcome
}

#[test]
fn buffered_error_response_arrives_whole_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = fixture("shared/error-keeps-headers.php");
    let rapira = Rapira::start(&tests::PHP_PARTS, Mode::Classic, script.clone(), None)?;
    let outcome = run_to_end(error_path_driver(), &rapira, script, Mode::Classic);
    drop(rapira);
    outcome
}

/// Output before the throw seals a truncated frame: the reply must end as truncated, not as a possibly-incomplete body.
#[test]
fn truncated_response_ends_as_truncated_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = fixture("shared/output-then-throw-worker.php");
    let rapira = Rapira::start(&tests::PHP_PARTS, Mode::Worker, script.clone(), None)?;
    let plugin = driver(|intake: Intake<Exchange>| async move {
        let err = match exchange(&intake, req("/")).await {
            Ok(resp) => anyhow::bail!(
                "the reply must end as truncated, got {} with body {:?}",
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
    });
    let outcome = run_to_end(plugin, &rapira, script, Mode::Worker);
    drop(rapira);
    outcome
}

/// A resident plugin serves until the stop flag, then returns.
#[test]
fn stop_ends_a_resident_plugin() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = fixture("plugin_tests/driver-classic.php");
    let rapira = Rapira::start(&tests::PHP_PARTS, Mode::Classic, script.clone(), None)?;
    let plugin = TestPlugin::boxed(|mut worker| {
        let handle = worker.handle.clone();
        handle.block_on(worker.stop.wait_for(|stop| *stop))?;
        Ok(())
    });
    let running = run_plugin(
        plugin,
        rapira.sink(),
        GRACE,
        DRAIN_GRACE,
        script,
        Mode::Classic,
    )?;

    let start = Instant::now();
    running.stop();
    let outcome = running.join();
    drop(rapira);
    outcome?;
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "graceful stop must not hang"
    );
    Ok(())
}

#[test]
fn many_plugins_run() -> anyhow::Result<()> {
    let _guard = php_lock();
    const N: usize = 12;
    let script = fixture("plugin_tests/driver-worker.php");
    let rapira = Rapira::start(&tests::PHP_PARTS, Mode::Worker, script.clone(), None)?;
    let running = (0..N)
        .map(|_| {
            run_plugin(
                two_exchanges(),
                rapira.sink(),
                GRACE,
                DRAIN_GRACE,
                script.clone(),
                Mode::Worker,
            )
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let outcomes: Vec<anyhow::Result<()>> = running.into_iter().map(|r| r.join()).collect();
    drop(rapira);
    assert!(
        outcomes.iter().all(|r| r.is_ok()),
        "some plugins failed: {outcomes:?}"
    );
    Ok(())
}

fn run_one(plugin: Box<TestPlugin>) -> anyhow::Result<anyhow::Result<()>> {
    let _guard = php_lock();
    let script = fixture("plugin_tests/driver-classic.php");
    let rapira = Rapira::start(&tests::PHP_PARTS, Mode::Classic, script.clone(), None)?;
    let outcome = run_to_end(plugin, &rapira, script, Mode::Classic);
    drop(rapira);
    Ok(outcome)
}

/// `serve` fails; join must surface the error.
#[test]
fn serve_returning_err_is_reported() -> anyhow::Result<()> {
    let outcome = run_one(TestPlugin::boxed(|_| Err(anyhow::anyhow!("boom"))))?;
    let err = outcome.unwrap_err().to_string();
    assert!(err.contains("boom"), "expected the serve error, got: {err}");
    Ok(())
}

/// `serve` panics; join must convert the panic into an error, not abort.
#[test]
fn panic_in_serve_is_reported() -> anyhow::Result<()> {
    let outcome = run_one(TestPlugin::boxed(|_| panic!("kaboom")))?;
    let err = outcome.unwrap_err().to_string();
    assert!(
        err.contains("test panicked: kaboom"),
        "expected a panic outcome, got: {err}"
    );
    Ok(())
}

/// A plugin that ignores the stop flag past the grace; join must give up at the grace.
#[test]
fn a_plugin_past_the_grace_is_reported() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = fixture("plugin_tests/driver-classic.php");
    let rapira = Rapira::start(&tests::PHP_PARTS, Mode::Classic, script.clone(), None)?;
    let (release, released) = std::sync::mpsc::channel::<()>();
    let plugin = TestPlugin::boxed(move |worker| {
        drop(worker);
        let _ = released.recv();
        Ok(())
    });
    let running = run_plugin(
        plugin,
        rapira.sink(),
        Duration::from_millis(100),
        DRAIN_GRACE,
        script,
        Mode::Classic,
    )?;
    let start = Instant::now();
    running.stop();
    let outcome = running.join();
    drop(release);
    drop(rapira);
    let err = outcome.unwrap_err().to_string();
    assert!(
        err.contains("did not stop within"),
        "expected a stop timeout, got: {err}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the join must be bounded by the grace, not hang"
    );
    Ok(())
}
