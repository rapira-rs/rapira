//! The observability thread serves `/metrics`, `/livez` and `/readyz`.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use rapira_config::Mode;
use rapira_net::ListenAddr;
use tests::wire::submit;
use tests::{Resp, StatusBody, drain_resp_deadline, poll, req};

use crate::harness::{
    Exited, Server, Spawn, diagnostics, fixture_path, free_port, http_get, http_raw, php_version,
    rapira_version, signal, wait_log_contains, wait_serving,
};

const REQ: Duration = Duration::from_secs(10);
/// `/?hang=1` holds the PHP thread. Other requests get a short response.
const HANG: &str = "lifecycle/hang-worker.php";
const PROBE_TYPE: &str = "text/plain; charset=utf-8";
const METRICS_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
const REQUESTS: &str = r#"rapira_requests_total{pool="http"}"#;
const CONFIGURED: &str = r#"rapira_workers_configured{pool="http"}"#;
const QUEUED: &str = r#"rapira_requests_queued{pool="http"}"#;
const STATES: [&str; 4] = ["starting", "idle", "active", "draining"];

fn workers(state: &str) -> String {
    format!(r#"rapira_workers{{pool="http",state="{state}"}}"#)
}

/// A server and the address of its `[observability]` listener.
struct ObservedServer {
    srv: Server,
    observability: SocketAddr,
}

/// Spawns one `[http]` pool of one worker over `fixture`, with `pool` keys in `[http.pool]` and an `[observability]` listener on a free port. `tables` follows the `listen` key: more `[observability]` keys, the sub-tables and other tables.
fn spawn(fixture: &str, pool: &str, tables: &str) -> ObservedServer {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(Mode::Dispatcher, fixture_path(fixture))
        .http_pool(pool)
        .toml(&format!(
            "[observability]\nlisten = \"{observability}\"\n{tables}\n"
        ))
        .spawn();
    poll(REQ, || {
        TcpStream::connect_timeout(&observability, Duration::from_millis(200)).ok()
    })
    .unwrap_or_else(|| panic!("observability listener did not open\n{}", diagnostics(&srv)));
    ObservedServer { srv, observability }
}

/// Sends one `GET` on its own connection.
fn get(addr: SocketAddr, path: &str) -> Resp {
    let mut rx = submit(addr, req(path)).unwrap_or_else(|e| panic!("GET {path}: {e}"));
    drain_resp_deadline(&mut rx, Instant::now() + REQ)
        .unwrap_or_else(|| panic!("GET {path}: no response within {REQ:?}"))
}

/// The samples of one `GET /metrics`.
fn scrape(addr: SocketAddr) -> BTreeMap<String, u64> {
    let resp = get(addr, "/metrics");
    assert_eq!(resp.status(), 200, "{}", resp.body_string());
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some(METRICS_TYPE),
        "{}",
        resp.body_string()
    );
    tests::metrics::samples(&resp.body_string())
}

/// The value of `series`. A missing series fails the test.
fn value(samples: &BTreeMap<String, u64>, series: &str) -> u64 {
    *samples
        .get(series)
        .unwrap_or_else(|| panic!("no series {series}\n{samples:#?}"))
}

/// Scrapes until `pred` holds, for at most 20 s.
fn scrape_until(
    srv: &Server,
    addr: SocketAddr,
    what: &str,
    pred: impl Fn(&BTreeMap<String, u64>) -> bool,
) -> BTreeMap<String, u64> {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let samples = scrape(addr);
        if pred(&samples) {
            return samples;
        }
        assert!(
            Instant::now() < end,
            "no {what} within 20 s\n{samples:#?}\n{}",
            diagnostics(srv)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The test sends three requests to one worker. The endpoint reports them for the `http` pool and leaves out the observability pool.
#[test]
fn a_scrape_reports_the_pool() {
    let ObservedServer { srv, observability } = spawn(HANG, "", "[observability.metrics]");
    wait_serving(&srv, "http", 1);
    for _ in 0..3 {
        let code = http_get(srv.addr, "/", REQ).expect("GET /").status;
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
    let samples = scrape_until(&srv, observability, "3 requests", |s| {
        value(s, REQUESTS) == 3
    });
    assert_eq!(value(&samples, CONFIGURED), 1);
    let total: u64 = STATES
        .iter()
        .map(|state| value(&samples, &workers(state)))
        .sum();
    assert_eq!(total, 1, "{samples:#?}");
    assert!(
        samples
            .keys()
            .all(|series| !series.contains(r#"pool="observability""#)),
        "{samples:#?}"
    );
    assert!(
        samples
            .keys()
            .all(|series| !series.starts_with("rapira_worker_exits_total")
                && !series.starts_with("rapira_worker_rss_bytes")
                && !series.starts_with("rapira_worker_pss_bytes")),
        "{samples:#?}"
    );
    let build = format!(
        r#"rapira_build_info{{version="{}",php_version="{}"}}"#,
        rapira_version(),
        php_version()
    );
    assert_eq!(value(&samples, &build), 1);
}

/// With only `[observability.metrics]`, each other route answers 404, including the probes.
#[test]
fn only_get_metrics_is_served() {
    struct Case {
        name: &'static str,
        request: &'static str,
    }
    let cases = [
        Case {
            name: "another path",
            request: "GET /other HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        },
        Case {
            name: "another method",
            request: "POST /metrics HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        },
        Case {
            name: "livez without the probes table",
            request: "GET /livez HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        },
        Case {
            name: "readyz without the probes table",
            request: "GET /readyz HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        },
    ];
    let ObservedServer { srv, observability } = spawn(HANG, "", "[observability.metrics]");
    for case in &cases {
        let status = http_raw(observability, case.request.as_bytes(), REQ)
            .expect(case.name)
            .status;
        assert_eq!(status, 404, "{}\n{}", case.name, diagnostics(&srv));
    }
}

/// With only `[observability.probes]`, `/livez` answers ok and `/metrics` answers 404.
#[test]
fn livez_answers_ok() {
    let ObservedServer { srv, observability } = spawn(HANG, "", "[observability.probes]");
    let resp = get(observability, "/livez");
    assert_eq!(
        (resp.status(), resp.body_string().as_str()),
        (200, "ok\n"),
        "\n{}",
        diagnostics(&srv)
    );
    assert_eq!(resp.header("content-type").as_deref(), Some(PROBE_TYPE));
    assert_eq!(get(observability, "/metrics").status(), 404);
}

/// An idle keep-alive connection closes after `keepalive_timeout_secs`. With 1 s the close comes within the 5 s read; the 60 s default misses it.
#[test]
fn an_idle_connection_closes_after_the_keepalive_timeout() {
    let ObservedServer {
        srv: _srv,
        observability,
    } = spawn(
        HANG,
        "",
        "keepalive_timeout_secs = 1\n[observability.metrics]",
    );
    let status = http_raw(
        observability,
        b"GET /metrics HTTP/1.1\r\nHost: e2e\r\n\r\n",
        Duration::from_secs(5),
    )
    .expect("the server closes the idle connection")
    .status;
    assert_eq!(status, 200);
}

/// The fixture waits for a file before its first pull. The pool becomes ready without client traffic.
#[test]
fn a_booting_worker_is_starting_and_not_ready_until_its_first_pull() {
    let ObservedServer { srv, observability } = spawn(
        "lifecycle/slow-boot-worker.php",
        "",
        "[observability.metrics]\n[observability.probes]",
    );
    let resp = get(observability, "/readyz");
    assert_eq!(
        (resp.status(), resp.body_string().as_str()),
        (503, "pool http: no ready worker\n"),
        "\n{}",
        diagnostics(&srv)
    );
    assert_eq!(resp.header("content-type").as_deref(), Some(PROBE_TYPE));
    let samples = scrape(observability);
    assert_eq!(
        (
            value(&samples, &workers("starting")),
            value(&samples, &workers("idle"))
        ),
        (1, 0),
        "starting and idle\n{samples:#?}\n{}",
        diagnostics(&srv)
    );
    std::fs::write(srv.dir.join("http/release-boot"), "").expect("allow the first pull");
    scrape_until(&srv, observability, "an idle worker", |s| {
        value(s, &workers("idle")) == 1 && value(s, &workers("starting")) == 0
    });
    // No request reaches the worker, so it stays idle.
    let resp = get(observability, "/readyz");
    assert_eq!((resp.status(), resp.body_string().as_str()), (200, "ok\n"));
}

/// A worker whose boot fails stays starting, because the shed pull of the host does not count as the first pull of the app. The 503 shows that the host shed the request in its shed pull. The slot stays starting through the re-boot and the next shed pull.
#[test]
fn a_worker_whose_boot_fails_stays_starting() {
    // No failure limit: with the default limit, the server stops 7.5 s after the first failed boot.
    let ObservedServer { srv, observability } = spawn(
        "lifecycle/never-loop-worker.php",
        "max_boot_failures = 0",
        "[observability.metrics]",
    );
    let code = http_get(srv.addr, "/", REQ).expect("GET /").status;
    assert_eq!(code, 503, "\n{}", diagnostics(&srv));
    let samples = scrape(observability);
    assert_eq!(
        (
            value(&samples, &workers("starting")),
            value(&samples, &workers("idle"))
        ),
        (1, 0),
        "starting and idle\n{samples:#?}\n{}",
        diagnostics(&srv)
    );
}

/// A pool whose boot failed turns ready without a request when a later boot succeeds. The fixture fails its boot until up.flag exists next to it.
#[test]
fn a_pool_whose_boot_failed_turns_ready_without_a_request() {
    // No failure limit: with the default limit, the server stops 7.5 s after the first failed boot.
    let ObservedServer { srv, observability } = spawn(
        "lifecycle/dependency-boot-worker.php",
        "max_boot_failures = 0",
        "[observability.probes]",
    );
    assert!(
        wait_log_contains(&srv, "dependency down", REQ),
        "\n{}",
        diagnostics(&srv)
    );
    let resp = get(observability, "/readyz");
    assert_eq!(
        (resp.status(), resp.body_string().as_str()),
        (503, "pool http: no ready worker\n"),
        "\n{}",
        diagnostics(&srv)
    );
    std::fs::write(srv.dir.join("http/up.flag"), "").expect("write up.flag");
    // No request goes to the app port, so only the worker itself can run the boot again.
    let resp = poll(Duration::from_secs(15), || {
        let resp = get(observability, "/readyz");
        (resp.status() == 200).then_some(resp)
    })
    .unwrap_or_else(|| {
        panic!(
            "the pool is not ready within 15 s after the boot can succeed\n{}",
            diagnostics(&srv)
        )
    });
    assert_eq!(resp.body_string(), "ok\n");
}

/// A failed re-boot after the app served shows starting, so the request watchdog skips the worker.
#[test]
fn a_failed_reboot_stays_starting() {
    let ObservedServer { srv, observability } = spawn(
        "lifecycle/reboot-fails-worker.php",
        "request_terminate_timeout_secs = 1\nmax_boot_failures = 1",
        "[observability.metrics]",
    );
    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("GET /");
    assert_eq!(
        (code, body.as_slice()),
        (200, b"ok".as_slice()),
        "\n{}",
        diagnostics(&srv)
    );
    assert!(
        wait_log_contains(&srv, "reboot failed", Duration::from_secs(10)),
        "\n{}",
        diagnostics(&srv)
    );
    // The sleep is longer than the 1 s limit plus the 1 s tick of the watchdog.
    std::thread::sleep(Duration::from_millis(2500));
    let samples = scrape(observability);
    assert_eq!(
        (
            value(&samples, &workers("starting")),
            value(&samples, &workers("active"))
        ),
        (1, 0),
        "starting and active\n{samples:#?}\n{}",
        diagnostics(&srv)
    );
}

/// The HTTP and observability listeners cannot share an address.
#[test]
fn an_observability_listener_on_the_http_address_fails_the_boot() {
    let tcp = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let Exited { status, log } = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .http_listen(ListenAddr::Tcp(tcp))
        .toml(&format!(
            "[observability]\nlisten = \"{tcp}\"\n[observability.metrics]\n"
        ))
        .boot_failure();
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(log.contains(&format!("bind {tcp}")), "{log}");
}

/// A PHP reload keeps the observability endpoint and its request totals available.
#[test]
fn observability_keeps_its_request_total_across_a_reload() {
    let ObservedServer { srv, observability } = spawn(HANG, "", "[observability.metrics]");
    wait_serving(&srv, "http", 1);
    assert_eq!(http_get(srv.addr, "/", REQ).expect("GET /").status, 200);
    scrape_until(&srv, observability, "one request", |s| {
        value(s, REQUESTS) == 1
    });
    signal(srv.pid(), libc::SIGUSR2);
    let samples = scrape_until(&srv, observability, "one recycle", |s| {
        value(s, r#"rapira_script_restarts_total{pool="http"}"#) == 1
    });
    assert_eq!(value(&samples, REQUESTS), 1);
}

/// The held request occupies the only PHP thread. The next two requests wait in the pool queue.
#[test]
fn requests_behind_a_held_worker_count_as_queued() {
    let ObservedServer { srv, observability } = spawn(
        HANG,
        "",
        "[observability.metrics]\n[observability.probes]\n[server]\nstop_timeout_secs = 1",
    );
    let addr = srv.addr;
    // These clients never get an answer. Their threads end when the server stops at the end of the test and the connections close.
    std::thread::spawn(move || http_get(addr, "/?hang=1", Duration::from_secs(60)));
    scrape_until(&srv, observability, "an active worker", |s| {
        value(s, &workers("active")) == 1
    });
    assert_eq!(get(observability, "/readyz").status(), 200);
    for _ in 0..2 {
        std::thread::spawn(move || http_get(addr, "/", Duration::from_secs(60)));
    }
    scrape_until(&srv, observability, "2 queued requests", |s| {
        value(s, QUEUED) == 2
    });
}
