use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use http::{HeaderMap, Method};
use http_body_util::BodyExt;
use rapira_config::Mode;
use serde_json::{Value, json};
use tests::grpc::{Conn, ECHO_PATH, HI_FRAME, WATCH_PATH, Wire, envelope};
use tests::wire::Frame;
use tests::{drain_async, fixture, req, server_log, wire};
use tokio::sync::mpsc::Receiver;
use tokio::time::timeout;

use crate::harness::{Server, Spawn, listen, logged_remote, stop, wait_serving};

const WAIT: Duration = Duration::from_secs(10);

/// Each GET opens a new HTTP/1.1 connection. The client can keep its response open.
async fn get(addr: SocketAddr, path: &str) -> anyhow::Result<Receiver<Frame>> {
    timeout(WAIT, wire::submit_async(addr, req(path))).await?
}

async fn http_view(response: Receiver<Frame>) -> anyhow::Result<Value> {
    let got = timeout(WAIT, drain_async(response)).await?;
    assert_eq!(got.status, 200, "{}", got.body);
    Ok(serde_json::from_str(&got.body)?)
}

fn grpc_view(headers: &HeaderMap) -> anyhow::Result<Value> {
    let view = headers.get("x-view").context("missing PHP view")?;
    Ok(serde_json::from_slice(view.as_bytes())?)
}

fn two_http_workers() -> Server {
    let srv = Spawn::http(Mode::Dispatcher, fixture("pool_threads/overlap/http.php"))
        .workers(2)
        .json_log()
        .spawn();
    wait_serving(&srv, "http", 2);
    srv
}

fn assert_two_apps(srv: &Server, first: &Value, second: &Value) {
    assert_eq!(first["pid"], srv.pid(), "first app: {first}");
    assert_eq!(second["pid"], srv.pid(), "second app: {second}");
    assert_ne!(
        first["app"].as_str().expect("first app token"),
        second["app"].as_str().expect("second app token"),
        "each app makes a token at boot"
    );
}

#[tokio::test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
async fn a_free_worker_answers_a_second_http_connection() -> anyhow::Result<()> {
    let srv = two_http_workers();
    let held = get(srv.addr, "/hold").await?;
    server_log::wait_app_record(&srv.log_file(), "/hold pending 0");

    let short = http_view(get(srv.addr, "/short").await?).await;
    std::fs::write(srv.dir.join("http/release"), [])?;
    let short = short.context("the short request did not finish before release")?;
    let held = http_view(held).await?;
    assert_eq!(short["target"], "/short");
    assert_eq!(held["target"], "/hold");
    assert_two_apps(&srv, &held, &short);
    Ok(())
}

#[tokio::test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
async fn the_worker_that_waited_last_answers_the_next_request() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("pool_threads/overlap/http.php"))
        .workers(4)
        .json_log()
        .spawn();
    wait_serving(&srv, "http", 4);
    let mut apps = Vec::new();
    for _ in 0..8 {
        let view = http_view(get(srv.addr, "/short").await?).await?;
        apps.push(view["app"].as_str().context("app token")?.to_owned());
        // The worker starts its next wait before the next request arrives.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let first = apps[0].clone();
    assert_eq!(
        apps,
        vec![first; 8],
        "one worker answers the requests that do not overlap"
    );
    Ok(())
}

#[tokio::test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
async fn pending_is_per_pool_and_active_is_per_worker() -> anyhow::Result<()> {
    let srv = two_http_workers();
    let log = srv.log_file();
    let first = get(srv.addr, "/hold-first").await?;
    server_log::wait_app_record(&log, "/hold-first pending 0");
    let second = get(srv.addr, "/hold-second").await?;
    server_log::wait_app_record(&log, "/hold-second pending 0");

    let queued = get(srv.addr, "/queued").await?;
    let first_sample = server_log::wait_app_record(&log, "/hold-first pending 1");
    let second_sample = server_log::wait_app_record(&log, "/hold-second pending 1");
    std::fs::write(srv.dir.join("http/release"), [])?;
    assert_eq!(
        serde_json::from_str::<Value>(&first_sample)?,
        json!({"active": 1})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&second_sample)?,
        json!({"active": 1})
    );
    assert_eq!(http_view(first).await?["target"], "/hold-first");
    assert_eq!(http_view(second).await?["target"], "/hold-second");
    assert_eq!(http_view(queued).await?["target"], "/queued");
    Ok(())
}

#[tokio::test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
async fn two_grpc_calls_overlap_on_one_http2_connection() -> anyhow::Result<()> {
    let srv = Spawn::grpc(fixture("pool_threads/overlap/grpc.php"))
        .workers(2)
        .spawn();
    wait_serving(&srv, "grpc", 2);
    let mut conn = timeout(WAIT, Conn::open(&listen(&srv), Wire::H2)).await??;
    let peer = logged_remote(conn.peer);
    let watch = timeout(
        WAIT,
        conn.request(
            Method::POST,
            WATCH_PATH,
            &[("content-type", "application/grpc"), ("te", "trailers")],
            HI_FRAME,
        ),
    )
    .await??;
    assert_eq!(watch.status(), 200);
    let held = grpc_view(watch.headers())?;
    let mut body = watch.into_body();
    let first = timeout(WAIT, async {
        let mut bytes = Vec::new();
        while bytes.len() < HI_FRAME.len() {
            let frame = body
                .frame()
                .await
                .context("Watch ended before its first message")??;
            bytes.extend_from_slice(frame.data_ref().context("expected Watch data")?);
        }
        anyhow::Ok(bytes)
    })
    .await??;
    assert_eq!(first, HI_FRAME);

    let echo = timeout(WAIT, conn.grpc(ECHO_PATH, &[], HI_FRAME)).await;
    std::fs::write(srv.dir.join("grpc/release"), [])?;
    let echo = echo.context("Echo did not finish while Watch held the first worker")??;
    assert_eq!(echo.grpc_status(), Some("0"), "{echo:?}");
    assert_eq!(echo.body.as_ref(), HI_FRAME);
    let short = grpc_view(&echo.headers)?;
    assert_eq!(held["peer"], peer);
    assert_eq!(short["peer"], peer);
    assert_two_apps(&srv, &held, &short);

    let end = timeout(WAIT, body.collect()).await??;
    assert_eq!(
        end.trailers()
            .and_then(|t| t.get("grpc-status"))
            .map(|v| v.as_bytes()),
        Some(b"0".as_slice())
    );
    assert_eq!(end.to_bytes().as_ref(), envelope(b"\x0a\x08released"));
    drop(conn);
    stop(srv).await;
    Ok(())
}

async fn echo_view(conn: &mut Conn, headers: &[(&str, &str)]) -> anyhow::Result<Value> {
    let got = timeout(WAIT, conn.grpc(ECHO_PATH, headers, HI_FRAME)).await??;
    assert_eq!(got.grpc_status(), Some("0"), "{got:?}");
    grpc_view(&got.headers)
}

#[tokio::test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
async fn http_and_grpc_keep_their_pool_settings_in_one_process() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        http_mode: Mode,
        mode_name: &'static str,
        dispatcher: Value,
    }
    let cases = [
        Case {
            name: "HTTP worker and gRPC dispatcher",
            http_mode: Mode::Worker,
            mode_name: "Worker",
            dispatcher: json!(r"Rapira\Exception\NoDispatcherError"),
        },
        Case {
            name: "HTTP dispatcher and gRPC dispatcher",
            http_mode: Mode::Dispatcher,
            mode_name: "Dispatcher",
            dispatcher: json!({"name": "http", "same": true}),
        },
    ];
    for case in cases {
        let srv = Spawn::http(case.http_mode, fixture("pool_threads/identity/http.php"))
            .with_grpc(fixture("pool_threads/identity/grpc.php"))
            .http_pool("env = { RAPIRA_POOL_VIEW = 'web', RAPIRA_HTTP_ONLY = 'h' }")
            .grpc_pool("env = { RAPIRA_POOL_VIEW = 'rpc', RAPIRA_GRPC_ONLY = 'g' }")
            .spawn();
        wait_serving(&srv, "http", 1);
        wait_serving(&srv, "grpc", 1);

        // The first Echo changes the gRPC app directory and environment before it answers.
        let mut conn = timeout(WAIT, Conn::open(&listen(&srv), Wire::H2)).await??;
        let grpc = echo_view(&mut conn, &[("x-change", "1")]).await?;
        let http = http_view(get(srv.addr, "/").await?).await?;
        let changed = echo_view(&mut conn, &[]).await?;
        let dir = srv.dir.canonicalize()?;
        assert_eq!(
            http,
            json!({
                "pid": srv.pid(), "mode": case.mode_name, "dispatcher": case.dispatcher,
                "cwd": dir.join("http"), "pool": "web", "http": "h", "grpc": false,
            }),
            "{}",
            case.name
        );
        assert_eq!(
            grpc,
            json!({
                "pid": srv.pid(), "mode": "Dispatcher", "dispatcher": {"name": "grpc", "same": true},
                "cwd": dir.join("grpc"), "pool": "rpc", "http": false, "grpc": "g",
            }),
            "{}",
            case.name
        );
        assert_eq!(changed["cwd"], json!(dir), "{}", case.name);
        assert_eq!(changed["pool"], "grpc app", "{}", case.name);
        drop(conn);
        stop(srv).await;
    }
    Ok(())
}

/// Checks that each pool has its own execution clock. The budget is 1 s, and each unit sleeps for 2 s. A sleep uses wall-clock time and no CPU time.
#[tokio::test]
#[cfg_attr(
    not(all(php_zts, php_execution_timers)),
    ignore = "needs ZTS PHP with per-thread execution timers"
)]
async fn http_and_grpc_keep_their_execution_clocks_in_one_process() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        http_pool: &'static str,
        grpc_pool: &'static str,
        http_completes: bool,
    }
    let cases = [
        Case {
            name: "only the HTTP pool uses the cpu clock",
            http_pool: "execution_clock = \"cpu\"",
            grpc_pool: "",
            http_completes: true,
        },
        Case {
            name: "only the gRPC pool uses the cpu clock",
            http_pool: "",
            grpc_pool: "execution_clock = \"cpu\"",
            http_completes: false,
        },
    ];
    let ini = std::fs::read_to_string(fixture("ini/timeout_tests/timeout.php.ini"))?;
    for case in cases {
        let srv = Spawn::http(
            Mode::Dispatcher,
            fixture("timeout_tests/timeout-dispatcher.php"),
        )
        .with_grpc(fixture("timeout_tests/sleep-grpc.php"))
        .http_pool(case.http_pool)
        .grpc_pool(case.grpc_pool)
        .php_ini(&ini)
        .spawn();
        wait_serving(&srv, "http", 1);
        wait_serving(&srv, "grpc", 1);

        let mut conn = timeout(WAIT, Conn::open(&listen(&srv), Wire::H2)).await??;
        // A killed call can end with an error status or with a reset stream.
        let grpc = timeout(WAIT, conn.grpc(ECHO_PATH, &[], HI_FRAME)).await?;
        let http = timeout(WAIT, drain_async(get(srv.addr, "/?sleep=2000").await?)).await?;
        assert_eq!(
            (
                http.status == 200,
                grpc.is_ok_and(|reply| reply.grpc_status() == Some("0"))
            ),
            (case.http_completes, !case.http_completes),
            "{}: (HTTP unit completed, gRPC call completed)",
            case.name
        );
        drop(conn);
        stop(srv).await;
    }
    Ok(())
}
