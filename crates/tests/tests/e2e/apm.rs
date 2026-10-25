//! CI-only APM suites: Linux CI installs newrelic.so and ddtrace.so into the extension dir without enabling them; each suite loads its agent through its own php.ini and skips when the .so is absent.

use std::collections::BTreeSet;
use std::process::Command;
use std::time::Duration;

use tests::StatusBody;

use crate::harness::{
    Server, assert_only_workers, diagnostics, fan_out, http_get, php_extension,
    spawn_with_phprc_and_config, wait_file_contains_all, wait_log_contains, wait_workers,
    worker_pids,
};

const REQ: Duration = Duration::from_secs(10);
const BOOT_POOL: Duration = Duration::from_secs(20);

/// A pool with the agent loaded: the 40-char dummy license passes the length-only local validation, and `dont_launch = 3` stops every daemon fork.
fn newrelic_srv(processes: usize, extra_toml: &str) -> Option<Server> {
    let so = php_extension("newrelic.so")?;
    // the relative logfile resolves against the scratch dir: the harness starts the child there for a cwd ini
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
    Some(spawn_with_phprc_and_config(
        "apm/newrelic-worker.php",
        processes,
        &ini,
        &format!("mode = \"worker\"\n{extra_toml}"),
    ))
}

fn probe(srv: &Server) -> String {
    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("GET /");
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(code, 200, "probe failed: {body:?}\n{}", diagnostics(srv));
    body
}

fn nr_pid(body: &str) -> u32 {
    let rest = body
        .strip_prefix("nr:ok:")
        .unwrap_or_else(|| panic!("unexpected body {body:?}"));
    let (version, pid) = rest
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("unexpected body {body:?}"));
    assert!(!version.is_empty(), "phpversion('newrelic') must be set");
    pid.parse()
        .unwrap_or_else(|_| panic!("bad pid in {body:?}"))
}

/// The daemon double-forks and reparents to init, so scan the whole process table and check its log stayed unwritten.
fn assert_no_daemon(srv: &Server) {
    let out = Command::new("ps")
        .args(["-axo", "command="])
        .output()
        .expect("ps");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("newrelic-daemon"),
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

/// Every newrelic MINIT branch returns SUCCESS and all newrelic_* functions register unconditionally: the pool must keep serving with no daemon and a dummy license.
#[test]
fn newrelic_loads_and_the_pool_keeps_serving() {
    let Some(srv) = newrelic_srv(2, "") else {
        return;
    };
    let before = wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    for _ in 0..20 {
        let pid = nr_pid(&probe(&srv));
        assert!(
            before.contains(&pid),
            "answering pid {pid} must be one of the workers {before:?}"
        );
    }
    assert_eq!(
        worker_pids(srv.pid()),
        before,
        "no worker was replaced under the agent\n{}",
        diagnostics(&srv)
    );
}

/// Late init runs at each worker's first RINIT; every agent log line carries "(<pid> <tid>)" (axiom/util_logging.c), so the pid, not the message text, is the assertion.
#[test]
fn newrelic_late_init_runs_in_every_worker() {
    let Some(srv) = newrelic_srv(2, "") else {
        return;
    };
    wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    probe(&srv);

    let pids: BTreeSet<u32> = fan_out(srv.addr, "/", 4, 25, nr_pid).into_iter().collect();
    assert!(
        pids.len() >= 2,
        "4 concurrent clients against 2 workers must reach both\n{}",
        diagnostics(&srv)
    );

    let needles: Vec<String> = pids
        .iter()
        .chain([srv.pid()].iter())
        .map(|p| format!("({p} "))
        .collect();
    if let Err(state) = wait_file_contains_all(
        &srv.dir.join("newrelic-agent.log"),
        &needles,
        Duration::from_secs(5),
    ) {
        panic!(
            "agent log must show the master (MINIT) and every serving worker (late init); \
             master {} workers {pids:?}\n{state}",
            srv.pid()
        );
    }
}

/// With `newrelic.daemon.dont_launch = 3` the agent never starts the daemon, from MINIT or RINIT.
#[test]
fn newrelic_launches_no_daemon_under_the_pool() {
    let Some(srv) = newrelic_srv(2, "") else {
        return;
    };
    let workers = wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    for _ in 0..10 {
        nr_pid(&probe(&srv));
    }
    assert_only_workers(&srv, &workers);
    assert_no_daemon(&srv);
}

/// The agent swaps zend_error_cb at late init but chains to the original: the 500, the recovery on the same interpreter, and the logged message must all survive.
#[test]
fn newrelic_uncaught_throw_path_is_unchanged() {
    let Some(srv) = newrelic_srv(1, "") else {
        return;
    };
    wait_workers(&srv, BOOT_POOL, "1 worker", |p| p.len() == 1);
    let before = nr_pid(&probe(&srv));
    let code = http_get(srv.addr, "/?boom=1", REQ)
        .expect("GET /?boom=1")
        .status;
    assert_eq!(code, 500, "{}", diagnostics(&srv));
    let after = nr_pid(&probe(&srv));
    assert_eq!(
        before,
        after,
        "the same interpreter must serve the follow-up: a pid change means a respawn\n{}",
        diagnostics(&srv)
    );
    assert!(
        wait_log_contains(&srv, "newrelic-worker: uncaught", Duration::from_secs(5)),
        "the exception text must reach the server log\n{}",
        diagnostics(&srv)
    );
}

/// max_requests = 2 is a quota of 3, so six probes cross an app restart: the agent keeps serving in the next request cycle of the same process.
#[test]
fn newrelic_survives_an_app_restart() {
    let Some(srv) = newrelic_srv(1, "max_requests = 2\n") else {
        return;
    };
    wait_workers(&srv, BOOT_POOL, "1 worker", |p| p.len() == 1);
    for _ in 0..6 {
        probe(&srv);
    }
}

/// No datadog.* keys: the SAPI check disables the tracer before sidecar, telemetry, or remote-config setup, so there is nothing to configure until upstream adds rapira to the list.
fn ddtrace_srv(processes: usize) -> Option<Server> {
    let so = php_extension("ddtrace.so")?;
    let ini = format!("extension = \"{}\"\ndisplay_errors = Off\n", so.display());
    Some(spawn_with_phprc_and_config(
        "apm/ddtrace-worker.php",
        processes,
        &ini,
        "mode = \"worker\"\n",
    ))
}

fn dd_field<'a>(body: &'a str, key: &str) -> &'a str {
    body.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .unwrap_or_else(|| panic!("no {key} line in {body:?}"))
}

fn dd_pid(body: &str) -> u32 {
    let disabled = dd_field(body, "dd:tracing") == "disabled";
    assert!(
        disabled,
        "the disabled state must not change (got {body:?})"
    );
    dd_field(body, "dd:pid")
        .parse()
        .unwrap_or_else(|_| panic!("bad pid in {body:?}"))
}

/// Neither of rapira's SAPI names is in the tracer's compatible list: MINIT must disable tracing and still load the extension; an `enabled` failure here means upstream added rapira.
#[test]
fn ddtrace_self_disables_under_the_rapira_sapi() {
    let Some(srv) = ddtrace_srv(2) else { return };
    wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
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
fn ddtrace_keeps_the_pool_serving_and_forks_nothing() {
    let Some(srv) = ddtrace_srv(2) else { return };
    let workers = wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    probe(&srv);
    let pids: BTreeSet<u32> = fan_out(srv.addr, "/", 4, 10, dd_pid).into_iter().collect();
    assert!(
        pids.len() >= 2,
        "4 concurrent clients against 2 workers must reach both\n{}",
        diagnostics(&srv)
    );
    assert!(
        pids.iter().all(|p| workers.contains(p)),
        "every answering pid must be a worker: {pids:?} vs {workers:?}"
    );
    assert_only_workers(&srv, &workers);
}
