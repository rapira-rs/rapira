//! APM suites for CI only. Linux CI installs newrelic.so and ddtrace.so into the extension dir and does not enable them. Each suite loads its agent through its own php.ini and skips when the .so is absent.

use std::collections::BTreeSet;
use std::process::Command;
use std::time::Duration;

use rapira_sapi::plugin::Mode;
use tests::StatusBody;

use crate::harness::{
    Server, Spawn, diagnostics, fan_out, fixture_path, http_get, is_zts, php_extension, slot_line,
    wait_file_contains_all, wait_log_contains, wait_serving,
};

const REQ: Duration = Duration::from_secs(10);

/// Starts a pool with the agent loaded. The 40-char dummy license passes the local validation, which checks only the length, and `dont_launch = 3` stops every daemon fork.
fn newrelic_srv(workers: usize, extra_toml: &str) -> Option<Server> {
    let so = php_extension("newrelic.so")?;
    // The agent opens its log in the server's launch directory at MINIT.
    let ini = format!(
        "extension = \"{}\"\n\
         newrelic.enabled = 1\n\
         newrelic.appname = \"rapira-e2e\"\n\
         newrelic.license = \"0123456789abcdef0123456789abcdef01234567\"\n\
         newrelic.daemon.dont_launch = 3\n\
         newrelic.loglevel = debug\n\
         newrelic.logfile = \"newrelic-agent.log\"\n\
         newrelic.daemon.logfile = \"newrelic-daemon.log\"\n\
         display_errors = Off\n",
        so.display()
    );
    Some(
        Spawn::http(Mode::Worker, fixture_path("apm/newrelic-worker.php"))
            .workers(workers)
            .php_ini(&ini)
            .http_pool(extra_toml)
            .spawn(),
    )
}

fn probe(srv: &Server) -> String {
    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("GET /");
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(code, 200, "probe failed: {body:?}\n{}", diagnostics(srv));
    body
}

fn nr_app(body: &str) -> String {
    let rest = body
        .strip_prefix("nr:ok:")
        .unwrap_or_else(|| panic!("unexpected body {body:?}"));
    let (version, app) = rest
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("unexpected body {body:?}"));
    assert!(!version.is_empty(), "phpversion('newrelic') must be set");
    assert_eq!(app.len(), 16, "app boot token in {body:?}");
    app.to_owned()
}

/// An agent may start a helper. A disabled helper must leave the server with no child processes.
fn assert_no_children(srv: &Server) {
    let out = Command::new("ps")
        .args(["-axo", "ppid="])
        .output()
        .expect("ps");
    assert!(out.status.success(), "ps failed: {}", out.status);
    let parents = String::from_utf8_lossy(&out.stdout);
    assert!(
        !parents.lines().any(|s| s.trim().parse() == Ok(srv.pid())),
        "the agent started a child process\n{}",
        diagnostics(srv)
    );
}

/// The New Relic daemon can reparent to init. Check its process name and its log.
fn assert_no_daemon(srv: &Server) {
    let out = Command::new("ps")
        .args(["-axo", "comm="])
        .output()
        .expect("ps");
    assert!(out.status.success(), "ps failed: {}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text
            .lines()
            .any(|s| std::path::Path::new(s.trim()).ends_with("newrelic-daemon")),
        "a newrelic-daemon process is running\n{}",
        diagnostics(srv)
    );
    let log = srv.dir.join("newrelic-daemon.log");
    assert!(
        std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "the daemon log must stay unwritten\n{}",
        diagnostics(srv)
    );
}

/// Every newrelic MINIT branch returns SUCCESS and all newrelic_* functions register unconditionally, so the pool must keep serving with no daemon and a dummy license.
#[test]
fn newrelic_loads_and_the_pool_keeps_serving() {
    let workers = if is_zts() { 2 } else { 1 };
    let Some(srv) = newrelic_srv(workers, "") else {
        return;
    };
    wait_serving(&srv, "http", workers);
    let apps: BTreeSet<_> = fan_out(srv.addr, "/", 4, 5, nr_app).into_iter().collect();
    assert_eq!(apps.len(), workers, "serving apps: {apps:?}");
    slot_line(&srv, "recycles=0 ");
}

/// The first RINIT runs late initialization once in the server process. https://github.com/newrelic/newrelic-php-agent/blob/v12.9.0.38/agent/php_rinit.c#L70-L79
#[test]
fn newrelic_initializes_the_module_and_the_first_request() {
    let Some(srv) = newrelic_srv(1, "") else {
        return;
    };
    wait_serving(&srv, "http", 1);
    nr_app(&probe(&srv));
    let needles = [
        "MINIT processing done".to_owned(),
        format!("late_init called from pid={}", srv.pid()),
    ];
    if let Err(state) = wait_file_contains_all(
        &srv.dir.join("newrelic-agent.log"),
        &needles,
        Duration::from_secs(5),
    ) {
        panic!("missing agent initialization record\n{state}");
    }
}

/// With `newrelic.daemon.dont_launch = 3` the agent never starts the daemon, from MINIT or RINIT.
#[test]
fn newrelic_launches_no_daemon_under_the_pool() {
    let Some(srv) = newrelic_srv(1, "") else {
        return;
    };
    wait_serving(&srv, "http", 1);
    for _ in 0..10 {
        nr_app(&probe(&srv));
    }
    assert_no_children(&srv);
    assert_no_daemon(&srv);
}

/// The agent swaps zend_error_cb at late init but chains to the original. The 500, the recovery on the same interpreter and the logged message must all survive.
#[test]
fn newrelic_uncaught_throw_path_is_unchanged() {
    let Some(srv) = newrelic_srv(1, "") else {
        return;
    };
    wait_serving(&srv, "http", 1);
    let before = nr_app(&probe(&srv));
    let code = http_get(srv.addr, "/?boom=1", REQ)
        .expect("GET /?boom=1")
        .status;
    assert_eq!(code, 500, "{}", diagnostics(&srv));
    let after = nr_app(&probe(&srv));
    assert_eq!(
        before,
        after,
        "the same app must serve the next request\n{}",
        diagnostics(&srv)
    );
    assert!(
        wait_log_contains(&srv, "newrelic-worker: uncaught", Duration::from_secs(5)),
        "the exception text must reach the server log\n{}",
        diagnostics(&srv)
    );
}

/// max_requests = 2 is a quota of 3, so six probes cross a recycle. The agent keeps serving in the next cycle of the same process.
#[test]
fn newrelic_survives_an_app_restart() {
    let Some(srv) = newrelic_srv(1, "max_requests = 2") else {
        return;
    };
    wait_serving(&srv, "http", 1);
    let apps: BTreeSet<_> = (0..6).map(|_| nr_app(&probe(&srv))).collect();
    assert_eq!(apps.len(), 2, "three requests per app: {apps:?}");
}

/// The config has no datadog.* keys. The SAPI check disables the tracer before the sidecar, telemetry and remote-config setup, so there is nothing to configure until upstream adds rapira to the list.
fn ddtrace_srv(workers: usize) -> Option<Server> {
    let so = php_extension("ddtrace.so")?;
    let ini = format!("extension = \"{}\"\ndisplay_errors = Off\n", so.display());
    Some(
        Spawn::http(Mode::Worker, fixture_path("apm/ddtrace-worker.php"))
            .workers(workers)
            .php_ini(&ini)
            .spawn(),
    )
}

fn dd_field<'a>(body: &'a str, key: &str) -> &'a str {
    body.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .unwrap_or_else(|| panic!("no {key} line in {body:?}"))
}

fn dd_app(body: &str) -> String {
    let disabled = dd_field(body, "dd:tracing") == "disabled";
    assert!(
        disabled,
        "the disabled state must not change (got {body:?})"
    );
    let app = dd_field(body, "dd:app");
    assert_eq!(app.len(), 16, "app boot token in {body:?}");
    app.to_owned()
}

/// Neither rapira SAPI name is in the compatible list of the tracer, so MINIT must disable tracing and still load the extension. An `enabled` failure here means that upstream added rapira.
#[test]
fn ddtrace_self_disables_under_the_rapira_sapi() {
    let Some(srv) = ddtrace_srv(1) else { return };
    wait_serving(&srv, "http", 1);
    let body = probe(&srv);
    assert_eq!(
        dd_field(&body, "dd:sapi_ok"),
        "1",
        "the SAPI name must match the PHP version's expected name (got {body:?})"
    );
    assert!(
        !dd_field(&body, "dd:version").is_empty(),
        "phpversion('ddtrace') must be set: the extension really loaded (got {body:?})"
    );
    assert_eq!(
        dd_field(&body, "dd:tracing"),
        "disabled",
        "phpinfo's \"Datadog tracing support\" row renders the disable flag (got {body:?})"
    );
    assert_eq!(
        dd_field(&body, "dd:active_span"),
        "NULL",
        "no root span may exist while the tracer is disabled (got {body:?})"
    );
}

/// The disabled tracer must stay disabled across requests and workers, keep the pool serving, and start no sidecar process.
#[test]
fn ddtrace_keeps_the_pool_serving_without_a_sidecar() {
    let workers = if is_zts() { 2 } else { 1 };
    let Some(srv) = ddtrace_srv(workers) else {
        return;
    };
    wait_serving(&srv, "http", workers);
    let apps: BTreeSet<_> = fan_out(srv.addr, "/", 4, 10, dd_app).into_iter().collect();
    assert_eq!(apps.len(), workers, "serving apps: {apps:?}");
    assert_no_children(&srv);
}
