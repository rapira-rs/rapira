//! The `[metrics]` endpoint: the process without PHP that serves `GET /metrics` from the worker scoreboard.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use rapira_net::ListenAddr;
use rapira_sapi::Mode;

use crate::harness::{
    Conn, Server, Spawn, diagnostics, fixture_path, free_port, http_get, http_get_raw, http_raw,
    parse_status_and_body, php_version, rapira_version, signal, wait_workers,
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

/// One `[http]` pool of one worker over the hang fixture, with `pool` keys in `[http.pool]` and a `[metrics]` listener on a free port.
fn spawn(pool: &str) -> (Server, SocketAddr) {
    let metrics = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .http_pool(pool)
        .toml(&format!("[metrics]\nlisten = \"{metrics}\""))
        .spawn();
    (srv, metrics)
}

/// One `GET /metrics`: the status, the head and the samples.
fn scrape(addr: SocketAddr) -> (u16, String, BTreeMap<String, u64>) {
    let raw = http_get_raw(addr, "/metrics", &[], REQ).expect("GET /metrics");
    let (status, body) = parse_status_and_body(&raw).expect("an HTTP response");
    let head = String::from_utf8_lossy(&raw[..raw.len() - body.len()]).into_owned();
    let text = std::str::from_utf8(body).expect("a UTF-8 body");
    (status, head, tests::metrics::samples(text))
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

/// Three requests on one worker. The endpoint reports them for the `http` pool and leaves out the metrics pool.
#[test]
fn a_scrape_reports_the_pool() {
    let (srv, metrics) = spawn("");
    for _ in 0..3 {
        let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
    let samples = scrape_until(&srv, metrics, "3 requests", |s| value(s, REQUESTS) == 3);

    let (status, head, _) = scrape(metrics);
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
            .all(|series| !series.contains(r#"pool="metrics""#)),
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
    let (srv, metrics) = spawn("");
    for case in &cases {
        let (status, _) = http_raw(metrics, case.request.as_bytes(), REQ).expect(case.name);
        assert_eq!(status, 404, "{}\n{}", case.name, diagnostics(&srv));
    }
}

/// An idle keep-alive connection closes after `keepalive_timeout_secs`. With 1 s the close comes within the 5 s read; the 60 s default misses it.
#[test]
fn an_idle_connection_closes_after_the_keepalive_timeout() {
    let metrics = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let _srv = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .toml(&format!(
            "[metrics]\nlisten = \"{metrics}\"\nkeepalive_timeout_secs = 1"
        ))
        .spawn();
    let mut conn = Conn::open(metrics, REQ).expect("connect");
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

/// The master binds every listener in one boot, the metrics listener first. The http pool then fails on the shared address.
#[test]
fn a_metrics_listener_on_the_http_address_fails_the_boot() {
    let tcp = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let (status, log) = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .http_listen(ListenAddr::Tcp(tcp))
        .toml(&format!("[metrics]\nlisten = \"{tcp}\""))
        .boot_failure();
    assert!(!status.success(), "{status:?}\n{log}");
    assert!(
        log.contains("plugin http: prepare failed") && log.contains(&format!("bind {tcp}")),
        "\n{log}"
    );
}

/// A USR2 reload replaces the metrics process too. The metrics process never pulls PHP work, so its slot must be idle from the bind: otherwise the reload gate of its pool waits the whole control timeout, 30 s by default.
#[test]
fn a_reload_replaces_the_metrics_process() {
    let (srv, metrics) = spawn("");
    let before = wait_workers(
        &srv,
        Duration::from_secs(20),
        "the worker and the metrics process",
        |p| p.len() == 2,
    );
    signal(srv.pid(), libc::SIGUSR2);
    wait_workers(
        &srv,
        Duration::from_secs(20),
        "a new worker and a new metrics process",
        |p| p.len() == 2 && p.iter().all(|pid| !before.contains(pid)),
    );
    let (status, _, _) = scrape(metrics);
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
    let (srv, metrics) = spawn("");
    let samples = scrape_until(&srv, metrics, "the memory of the worker", |s| {
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
    let (srv, metrics) = spawn("max_requests = 2");
    for _ in 0..3 {
        let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
    // The third request reaches the quota. The master counts the exit before it forks the new worker, so the next requests cannot reach the draining worker.
    scrape_until(&srv, metrics, "the recycle", |s| {
        value(s, &exits("recycled")) == 1
    });
    for _ in 0..2 {
        let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
    let samples = scrape_until(&srv, metrics, "5 requests and 1 recycle", |s| {
        value(s, REQUESTS) == 5 && value(s, &exits("recycled")) == 1
    });
    assert_eq!(value(&samples, &exits("crashed")), 0);
}

/// A worker that a signal kills, and that the master did not signal, counts as crashed.
#[test]
fn a_killed_worker_counts_as_crashed() {
    let (srv, metrics) = spawn("");
    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    let pid: u32 = String::from_utf8(body)
        .expect("a UTF-8 body")
        .strip_prefix("ok:")
        .and_then(|pid| pid.parse().ok())
        .expect("the fixture answers ok:<pid>");
    signal(pid, libc::SIGKILL);
    scrape_until(&srv, metrics, "1 crashed exit", |s| {
        value(s, &exits("crashed")) == 1
    });
}

/// The held request takes the only PHP thread, so the next two requests wait in the worker queue.
#[test]
fn requests_behind_a_held_worker_count_as_queued() {
    let (srv, metrics) = spawn("");
    let addr = srv.addr;
    // These clients never get an answer. Their threads end when the server stops at the end of the test and the connections close.
    std::thread::spawn(move || http_get(addr, "/?hang=1", Duration::from_secs(60)));
    scrape_until(&srv, metrics, "an active worker", |s| {
        value(s, &workers("active")) == 1
    });
    for _ in 0..2 {
        std::thread::spawn(move || http_get(addr, "/", Duration::from_secs(60)));
    }
    scrape_until(&srv, metrics, "2 queued requests", |s| {
        value(s, QUEUED) == 2
    });
}
