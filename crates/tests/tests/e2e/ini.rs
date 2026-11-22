use crate::harness::{self, Spawn, fixture_path, http_get};
use rapira_sapi::plugin::Mode;
use std::time::Duration;
use tests::StatusBody;

const REQ: Duration = Duration::from_secs(10);

#[test]
fn php_ini_in_the_working_directory_is_ignored() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path("ini/precision.php"))
        .php_ini("precision = 5\n")
        .no_phprc()
        .spawn();
    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("precision=14"),
        "a php.ini in the cwd must not apply (got {body:?})\n{}",
        harness::diagnostics(&srv)
    );
}

/// Control for the test above. It proves that the fixture reads the ini, so a passing cwd test is not vacuous.
#[test]
fn the_same_file_applies_through_phprc() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path("ini/precision.php"))
        .php_ini("precision = 5\n")
        .spawn();
    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("precision=5"),
        "PHPRC must apply (got {body:?})\n{}",
        harness::diagnostics(&srv)
    );
}

#[test]
fn a_process_timer_can_only_run_with_one_worker() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        workers: usize,
        run: bool,
        disabled: bool,
    }
    let cases = [
        Case {
            name: "one worker uses the configured limits",
            workers: 1,
            run: true,
            disabled: false,
        },
        Case {
            name: "two workers use separate timers",
            workers: 2,
            run: rapira_sapi::zts() && rapira_sapi::has_execution_timers(),
            disabled: false,
        },
        Case {
            name: "two workers cannot enable the process timer",
            workers: 2,
            run: rapira_sapi::zts() && !rapira_sapi::has_execution_timers(),
            disabled: true,
        },
    ];
    for case in cases.into_iter().filter(|case| case.run) {
        let srv = Spawn::http(Mode::Worker, fixture_path("ini/timeouts.php"))
            .workers(case.workers)
            .php_ini("max_execution_time=9\nmax_input_time=8\nopcache.enable=0\n")
            .spawn();
        let response = http_get(srv.addr, "/", REQ)?;
        assert_eq!(response.status, 200, "{}", case.name);
        let expected = if case.disabled {
            serde_json::json!(["0", "-1", false, false, "0"])
        } else {
            serde_json::json!(["9", "8", "9", true, "6"])
        };
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response.body)?,
            expected,
            "{}",
            case.name
        );
        assert_eq!(
            std::fs::read_to_string(srv.log_file())?.contains("no per-thread execution timers"),
            case.disabled,
            "{}: startup warning",
            case.name
        );
    }
    Ok(())
}
