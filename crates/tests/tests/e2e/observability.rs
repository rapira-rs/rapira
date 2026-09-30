//! The `[observability]` endpoint: the process without PHP that serves `GET /metrics`, `GET /livez` and `GET /readyz`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use rapira_net::ListenAddr;
use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{Resp, drain_resp_deadline, req};

use crate::harness::{
    Server, Spawn, diagnostics, fixture_path, free_port, http_get, http_raw, php_version,
    rapira_version, serving_pid, signal, wait_log_contains, wait_workers,
};
#[cfg(target_os = "linux")]
use crate::harness::{kill_master_as_subreaper, wait_child_exit};

const REQ: Duration = Duration::from_secs(10);
/// Dispatcher mode. `/` answers `ok:<pid>`, and `/?hang=1` holds the PHP thread forever.
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

fn exits(reason: &str) -> String {
    format!(r#"rapira_worker_exits_total{{pool="http",reason="{reason}"}}"#)
}

/// One `[http]` pool of one worker over `fixture`, with `pool` keys in `[http.pool]` and an `[observability]` listener on a free port. `tables` follows the `listen` key: more `[observability]` keys, the sub-tables and other tables.
fn spawn(fixture: &str, pool: &str, tables: &str) -> (Server, SocketAddr) {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::http(Mode::Dispatcher, fixture_path(fixture))
        .http_pool(pool)
        .toml(&format!(
            "[observability]\nlisten = \"{observability}\"\n{tables}\n"
        ))
        .spawn();
    (srv, observability)
}

/// One `GET` on its own connection.
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

/// Three requests on one worker. The endpoint reports them for the `http` pool and leaves out the observability pool.
#[test]
fn a_scrape_reports_the_pool() {
    let (srv, observability) = spawn(HANG, "", "[observability.metrics]");
    for _ in 0..3 {
        let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
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
    let build = format!(
        r#"rapira_build_info{{version="{}",php_version="{}"}}"#,
        rapira_version(),
        php_version()
    );
    assert_eq!(value(&samples, &build), 1);
}

/// With only `[observability.metrics]`, each other route answers 404, the probes too.
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
    let (srv, observability) = spawn(HANG, "", "[observability.metrics]");
    for case in &cases {
        let (status, _) = http_raw(observability, case.request.as_bytes(), REQ).expect(case.name);
        assert_eq!(status, 404, "{}\n{}", case.name, diagnostics(&srv));
    }
}

/// With only `[observability.probes]`, `/livez` answers ok and `/metrics` answers 404.
#[test]
fn livez_answers_ok() {
    let (srv, observability) = spawn(HANG, "", "[observability.probes]");
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
    let (_srv, observability) = spawn(
        HANG,
        "",
        "keepalive_timeout_secs = 1\n[observability.metrics]",
    );
    let (status, _) = http_raw(
        observability,
        b"GET /metrics HTTP/1.1\r\nHost: e2e\r\n\r\n",
        Duration::from_secs(5),
    )
    .expect("the server closes the idle connection");
    assert_eq!(status, 200);
}

/// A booting worker shows starting until its first pull, and its pool is not ready. The fixture sleeps 3 s before its first receive().
#[test]
fn a_booting_worker_is_starting_and_not_ready_until_its_first_pull() {
    let (srv, observability) = spawn(
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
    scrape_until(&srv, observability, "an idle worker", |s| {
        value(s, &workers("idle")) == 1 && value(s, &workers("starting")) == 0
    });
    // No request reaches the worker, so it stays idle.
    let resp = get(observability, "/readyz");
    assert_eq!((resp.status(), resp.body_string().as_str()), (200, "ok\n"));
}

/// A worker whose boot fails stays starting: the host's shed pull does not count as the app's first pull. The 503 shows that the host shed the request in its shed pull. The slot stays starting through the re-boot and the next shed pull.
#[test]
fn a_worker_whose_boot_fails_stays_starting() {
    let (srv, observability) = spawn(
        "lifecycle/never-loop-worker.php",
        "",
        "[observability.metrics]",
    );
    let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
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
    let (srv, observability) = spawn(
        "lifecycle/dependency-boot-worker.php",
        "",
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
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = get(observability, "/readyz");
        if resp.status() == 200 {
            assert_eq!(resp.body_string(), "ok\n");
            break;
        }
        assert!(
            Instant::now() < end,
            "the pool is not ready within 15 s after the boot can succeed\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A failed re-boot after the app served shows starting, so the request watchdog skips the worker.
#[test]
fn a_failed_reboot_stays_starting() {
    let (srv, observability) = spawn(
        "lifecycle/reboot-fails-worker.php",
        "request_terminate_timeout_secs = 1",
        "[observability.metrics]",
    );
    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
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
    // Longer than the 1 s limit plus the 1 s tick of the watchdog.
    std::thread::sleep(Duration::from_millis(2500));
    let samples = scrape(observability);
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

/// A USR2 reload replaces the observability process too. It never pulls PHP work, so it stores idle itself: otherwise the reload gate of its pool waits the whole control timeout, 30 s by default.
#[test]
fn a_reload_replaces_the_observability_process() {
    let (srv, observability) = spawn(HANG, "", "[observability.metrics]");
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
    assert_eq!(get(observability, "/metrics").status(), 200);
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
    let (srv, observability) = spawn(HANG, "", "[observability.metrics]");
    let samples = scrape_until(&srv, observability, "the memory of the worker", |s| {
        s.contains_key(r#"rapira_worker_pss_bytes{pool="http",worker="0"}"#)
    });
    for case in &cases {
        let prefix = format!("{}{{", case.family);
        let series: Vec<(&String, &u64)> = samples
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
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
    let (srv, observability) = spawn(HANG, "max_requests = 2", "[observability.metrics]");
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
    let (srv, observability) = spawn(HANG, "", "[observability.metrics]");
    signal(serving_pid(&srv), libc::SIGKILL);
    scrape_until(&srv, observability, "1 crashed exit", |s| {
        value(s, &exits("crashed")) == 1
    });
}

/// The held request takes the only PHP thread, so the next two requests wait in the worker queue. The held request never ends, so a 1 s control timeout ends the stop at the end of the test.
#[test]
fn requests_behind_a_held_worker_count_as_queued() {
    let (srv, observability) = spawn(
        HANG,
        "",
        "[observability.metrics]\n[supervisor]\nprocess_control_timeout_secs = 1",
    );
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

/// The master dies while its worker holds a request, and the orphaned worker drains for up to 55 s. A new server on the same observability address boots and answers in that time. The test process is a child subreaper while the master dies, so the orphaned processes become its children. https://man7.org/linux/man-pages/man2/PR_SET_CHILD_SUBREAPER.2const.html
#[cfg(target_os = "linux")]
#[test]
fn a_new_server_takes_the_observability_address_while_an_orphan_drains() {
    let (mut first, observability) = spawn(
        HANG,
        "",
        "[observability.metrics]\n[supervisor]\nprocess_control_timeout_secs = 60",
    );
    let pids = wait_workers(
        &first,
        Duration::from_secs(20),
        "the worker and the observability process",
        |p| p.len() == 2,
    );
    let worker = serving_pid(&first);
    let obs: u32 = *pids
        .iter()
        .find(|&&pid| pid != worker)
        .expect("the observability process");
    let addr = first.addr;
    // The client never gets an answer. Its thread ends when the test kills the orphan.
    std::thread::spawn(move || http_get(addr, "/?hang=1", Duration::from_secs(120)));
    scrape_until(&first, observability, "an active worker", |s| {
        value(s, &workers("active")) == 1
    });

    kill_master_as_subreaper(&mut first);
    // The first observability process closes its own listener when it exits.
    wait_child_exit(obs, Duration::from_secs(10), &first);

    let second = Spawn::http(Mode::Dispatcher, fixture_path(HANG))
        .toml(&format!(
            "[observability]\nlisten = \"{observability}\"\n[observability.probes]\n"
        ))
        .spawn();
    let resp = get(observability, "/livez");
    assert_eq!(
        (resp.status(), resp.body_string().as_str()),
        (200, "ok\n"),
        "\n{}",
        diagnostics(&second)
    );
    let mut status: libc::c_int = 0;
    // SAFETY: non-blocking waitpid on a child of this process; status is a live out-param.
    let rc = unsafe { libc::waitpid(worker as libc::pid_t, &mut status, libc::WNOHANG) };
    assert_eq!(rc, 0, "the orphaned worker {worker} stopped draining");

    signal(worker, libc::SIGKILL);
    wait_child_exit(worker, Duration::from_secs(10), &first);
}
