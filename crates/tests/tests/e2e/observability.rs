//! The `[observability]` endpoint: the process without PHP that serves `GET /metrics`, `GET /livez` and `GET /readyz`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use rapira_net::ListenAddr;
use rapira_sapi::Mode;

use crate::harness::{
    Conn, Server, Spawn, diagnostics, fixture_path, free_port, http_get, http_get_raw, http_raw,
    parse_status_and_body, php_version, rapira_version, signal, wait_log_contains, wait_workers,
};

const REQ: Duration = Duration::from_secs(10);
/// Dispatcher mode. `/` answers `ok:<pid>`, and `/?hang=1` holds the PHP thread forever.
const HANG: &str = "lifecycle/hang-worker.php";
const REQUESTS: &str = r#"rapira_requests_total{pool="http"}"#;
const CONFIGURED: &str = r#"rapira_workers_configured{pool="http"}"#;
const QUEUED: &str = r#"rapira_requests_queued{pool="http"}"#;
const STATES: [&str; 4] = ["starting", "idle", "active", "draining"];

fn workers(state: &str) -> String {
    format!(r#"rapira_workers{{pool="http",state="{state}"}}"#)
}

fn exits(reason: &str) -> String {
    format!(r#"rapira_worker_exits_total{{pool="http",reason="{reason}"}}"#)
}

/// One `[http]` pool of one worker over the hang fixture, with `pool` keys in `[http.pool]` and an `[observability]` listener on a free port.
fn spawn(pool: &str) -> (Server, SocketAddr) {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .http_pool(pool)
        .toml(&format!(
            "[observability]\nlisten = \"{observability}\"\n[observability.metrics]\n"
        ))
        .spawn();
    (srv, observability)
}

/// One `GET`: the status, the head and the body.
fn get(addr: SocketAddr, path: &str) -> (u16, String, String) {
    let raw = http_get_raw(addr, path, &[], REQ).unwrap_or_else(|e| panic!("GET {path}: {e}"));
    let (status, body) = parse_status_and_body(&raw).expect("an HTTP response");
    let head = String::from_utf8_lossy(&raw[..raw.len() - body.len()]).into_owned();
    let body = String::from_utf8(body.to_vec()).expect("a UTF-8 body");
    (status, head, body)
}

/// One `GET /metrics`: the status, the head and the samples.
fn scrape(addr: SocketAddr) -> (u16, String, BTreeMap<String, u64>) {
    let (status, head, body) = get(addr, "/metrics");
    (status, head, tests::metrics::samples(&body))
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
        let (_, _, samples) = scrape(addr);
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

/// Three requests on one worker. The endpoint reports them for the `http` pool and leaves out the observability pool.
#[test]
fn a_scrape_reports_the_pool() {
    let (srv, observability) = spawn("");
    for _ in 0..3 {
        let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
    let samples = scrape_until(&srv, observability, "3 requests", |s| {
        value(s, REQUESTS) == 3
    });

    let (status, head, _) = scrape(observability);
    assert_eq!(status, 200, "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("\r\ncontent-type: text/plain; version=0.0.4; charset=utf-8\r\n"),
        "{head}"
    );
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
    let build = format!(
        r#"rapira_build_info{{version="{}",php_version="{}"}}"#,
        rapira_version(),
        php_version()
    );
    assert_eq!(value(&samples, &build), 1);
}

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
    ];
    let (srv, observability) = spawn("");
    for case in &cases {
        let (status, _) = http_raw(observability, case.request.as_bytes(), REQ).expect(case.name);
        assert_eq!(status, 404, "{}\n{}", case.name, diagnostics(&srv));
    }
}

#[test]
fn livez_answers_ok() {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .toml(&format!(
            "[observability]\nlisten = \"{observability}\"\n[observability.probes]\n"
        ))
        .spawn();
    let (status, head, body) = get(observability, "/livez");
    assert_eq!(
        (status, body.as_str()),
        (200, "ok\n"),
        "\n{}",
        diagnostics(&srv)
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("\r\ncontent-type: text/plain; charset=utf-8\r\n"),
        "{head}"
    );
}

/// The slot of a booting worker stays starting until its first pull, so its pool is not ready. The fixture sleeps 3 s before its first receive().
#[test]
fn readyz_waits_for_the_first_pull() {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/slow-boot-worker.php"),
    )
    .toml(&format!(
        "[observability]\nlisten = \"{observability}\"\n[observability.metrics]\n[observability.probes]\n"
    ))
    .spawn();
    let (status, head, body) = get(observability, "/readyz");
    assert_eq!(
        (status, body.as_str()),
        (503, "pool http: no ready worker\n"),
        "\n{}",
        diagnostics(&srv)
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("\r\ncontent-type: text/plain; charset=utf-8\r\n"),
        "{head}"
    );
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let (status, _, body) = get(observability, "/readyz");
        if (status, body.as_str()) == (200, "ok\n") {
            break;
        }
        assert!(
            Instant::now() < end,
            "no ready pool within 20 s: {status} {body:?}\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Each sub-table turns on its own routes. A route of a missing sub-table answers 404.
#[test]
fn each_endpoint_group_needs_its_table() {
    struct Case {
        name: &'static str,
        table: &'static str,
        /// A route of the configured sub-table.
        served: &'static str,
        /// A route of the missing sub-table.
        missing: &'static str,
    }
    let cases = [
        Case {
            name: "probes only",
            table: "[observability.probes]",
            served: "/livez",
            missing: "/metrics",
        },
        Case {
            name: "metrics only",
            table: "[observability.metrics]",
            served: "/metrics",
            missing: "/livez",
        },
    ];
    for case in &cases {
        let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
        let srv = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
            .toml(&format!(
                "[observability]\nlisten = \"{observability}\"\n{}\n",
                case.table
            ))
            .spawn();
        let (served, _, _) = get(observability, case.served);
        let (missing, _, _) = get(observability, case.missing);
        assert_eq!(
            (served, missing),
            (200, 404),
            "{}\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
}

/// An idle keep-alive connection closes after `keepalive_timeout_secs`. With 1 s the close comes within the 5 s read; the 60 s default misses it.
#[test]
fn an_idle_connection_closes_after_the_keepalive_timeout() {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let _srv = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .toml(&format!(
            "[observability]\nlisten = \"{observability}\"\nkeepalive_timeout_secs = 1\n[observability.metrics]\n"
        ))
        .spawn();
    let mut conn = Conn::open(observability, REQ).expect("connect");
    conn.send(b"GET /metrics HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let (status, fields) = conn.read_head(REQ).expect("head");
    assert_eq!(status, 200);
    let len: usize = fields
        .iter()
        .find(|(k, _)| k == "content-length")
        .map(|(_, v)| v.parse().expect("a content-length number"))
        .expect("a content-length field");
    conn.read_n(len, REQ).expect("the body");
    let rest = conn
        .read_remaining(Duration::from_secs(5))
        .expect("the server closes the idle connection");
    assert!(rest.is_empty(), "{rest:?}");
}

/// A booting worker shows starting until its first pull. The fixture sleeps 3 s before its first receive().
#[test]
fn a_booting_worker_shows_starting_until_its_first_pull() {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/slow-boot-worker.php"),
    )
    .toml(&format!(
        "[observability]\nlisten = \"{observability}\"\n[observability.metrics]\n"
    ))
    .spawn();
    let (_, _, samples) = scrape(observability);
    assert_eq!(
        (
            value(&samples, &workers("starting")),
            value(&samples, &workers("idle"))
        ),
        (1, 0),
        "starting and idle\n{samples:#?}\n{}",
        diagnostics(&srv)
    );
    scrape_until(&srv, observability, "an idle worker", |s| {
        value(s, &workers("idle")) == 1 && value(s, &workers("starting")) == 0
    });
}

/// A worker whose boot fails stays starting: the host's shed pull does not count as the app's first pull.
#[test]
fn a_worker_whose_boot_fails_stays_starting() {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/never-loop-worker.php"),
    )
    .toml(&format!(
        "[observability]\nlisten = \"{observability}\"\n[observability.metrics]\n"
    ))
    .spawn();
    assert!(
        wait_log_contains(&srv, "booted", Duration::from_secs(10)),
        "\n{}",
        diagnostics(&srv)
    );
    // After the log call, the cycle fails and the host waits in the shed pull.
    std::thread::sleep(Duration::from_millis(500));
    let (_, _, samples) = scrape(observability);
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

/// A failed re-boot after the app served shows starting, so the request watchdog skips the worker.
#[test]
fn a_failed_reboot_stays_starting() {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/reboot-fails-worker.php"),
    )
    .http_pool("request_terminate_timeout_secs = 1")
    .toml(&format!(
        "[observability]\nlisten = \"{observability}\"\n[observability.metrics]\n"
    ))
    .spawn();
    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
    assert_eq!(
        (code, body),
        (200, b"ok".to_vec()),
        "\n{}",
        diagnostics(&srv)
    );
    assert!(
        wait_log_contains(&srv, "reboot failed", Duration::from_secs(10)),
        "\n{}",
        diagnostics(&srv)
    );
    // Longer than the 1 s limit plus the 1 s tick of the watchdog.
    std::thread::sleep(Duration::from_millis(2500));
    let (_, _, samples) = scrape(observability);
    assert_eq!(
        (
            value(&samples, &workers("starting")),
            value(&samples, &workers("active")),
            value(&samples, &exits("timeout"))
        ),
        (1, 0, 0),
        "starting, active and timeout exits\n{samples:#?}\n{}",
        diagnostics(&srv)
    );
}

/// The master binds every listener in one boot, the observability listener first. The http pool then fails on the shared address.
#[test]
fn an_observability_listener_on_the_http_address_fails_the_boot() {
    let tcp = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let (status, log) = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .http_listen(ListenAddr::Tcp(tcp))
        .toml(&format!(
            "[observability]\nlisten = \"{tcp}\"\n[observability.metrics]\n"
        ))
        .boot_failure();
    assert!(!status.success(), "{status:?}\n{log}");
    assert!(
        log.contains("plugin http: prepare failed") && log.contains(&format!("bind {tcp}")),
        "\n{log}"
    );
}

/// A USR2 reload replaces the observability process too. It never pulls PHP work, so it stores idle itself after the bind: otherwise the reload gate of its pool waits the whole control timeout, 30 s by default.
#[test]
fn a_reload_replaces_the_observability_process() {
    let (srv, observability) = spawn("");
    let before = wait_workers(
        &srv,
        Duration::from_secs(20),
        "the worker and the observability process",
        |p| p.len() == 2,
    );
    signal(srv.pid(), libc::SIGUSR2);
    wait_workers(
        &srv,
        Duration::from_secs(20),
        "a new worker and a new observability process",
        |p| p.len() == 2 && p.iter().all(|pid| !before.contains(pid)),
    );
    let (status, _, _) = scrape(observability);
    assert_eq!(status, 200);
}

/// Linux only: the endpoint reads `/proc/<pid>/smaps_rollup` of each live worker.
#[cfg(target_os = "linux")]
#[test]
fn each_live_worker_reports_its_memory() {
    struct Case {
        name: &'static str,
        family: &'static str,
    }
    let cases = [
        Case {
            name: "rss",
            family: "rapira_worker_rss_bytes",
        },
        Case {
            name: "pss",
            family: "rapira_worker_pss_bytes",
        },
    ];
    let (srv, observability) = spawn("");
    let samples = scrape_until(&srv, observability, "the memory of the worker", |s| {
        s.contains_key(r#"rapira_worker_pss_bytes{pool="http",worker="0"}"#)
    });
    for case in &cases {
        let series: Vec<(&String, &u64)> = samples
            .iter()
            .filter(|(k, _)| k.starts_with(&format!("{}{{", case.family)))
            .collect();
        assert_eq!(
            series.len(),
            1,
            "{}: one worker, one series: {series:?}",
            case.name
        );
        assert_eq!(
            series[0].0,
            &format!(r#"{}{{pool="http",worker="0"}}"#, case.family),
            "{}",
            case.name
        );
        assert!(*series[0].1 > 0, "{}: {series:?}", case.name);
    }
}

/// max_requests = 2 gives a quota of exactly 3: effective_quota adds 1 + hash % max(2 / 2, 1). Five requests recycle the worker once.
#[test]
fn counts_survive_a_recycle() {
    let (srv, observability) = spawn("max_requests = 2");
    for _ in 0..3 {
        let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
    // The third request reaches the quota. The master counts the exit before it forks the new worker, so the next requests cannot reach the draining worker.
    scrape_until(&srv, observability, "the recycle", |s| {
        value(s, &exits("recycled")) == 1
    });
    for _ in 0..2 {
        let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
    let samples = scrape_until(&srv, observability, "5 requests and 1 recycle", |s| {
        value(s, REQUESTS) == 5 && value(s, &exits("recycled")) == 1
    });
    assert_eq!(value(&samples, &exits("crashed")), 0);
}

/// A worker that a signal kills, and that the master did not signal, counts as crashed.
#[test]
fn a_killed_worker_counts_as_crashed() {
    let (srv, observability) = spawn("");
    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    let pid: u32 = String::from_utf8(body)
        .expect("a UTF-8 body")
        .strip_prefix("ok:")
        .and_then(|pid| pid.parse().ok())
        .expect("the fixture answers ok:<pid>");
    signal(pid, libc::SIGKILL);
    scrape_until(&srv, observability, "1 crashed exit", |s| {
        value(s, &exits("crashed")) == 1
    });
}

/// The held request takes the only PHP thread, so the next two requests wait in the worker queue.
#[test]
fn requests_behind_a_held_worker_count_as_queued() {
    let (srv, observability) = spawn("");
    let addr = srv.addr;
    // These clients never get an answer. Their threads end when the server stops at the end of the test and the connections close.
    std::thread::spawn(move || http_get(addr, "/?hang=1", Duration::from_secs(60)));
    scrape_until(&srv, observability, "an active worker", |s| {
        value(s, &workers("active")) == 1
    });
    for _ in 0..2 {
        std::thread::spawn(move || http_get(addr, "/", Duration::from_secs(60)));
    }
    scrape_until(&srv, observability, "2 queued requests", |s| {
        value(s, QUEUED) == 2
    });
}
