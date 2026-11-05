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

/// Control for the test above: proves the fixture does read the ini, so a green cwd test is not vacuous.
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
