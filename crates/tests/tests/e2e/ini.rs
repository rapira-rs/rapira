use crate::harness::{self, http_get};
use std::time::Duration;
use tests::StatusBody;

const REQ: Duration = Duration::from_secs(10);

#[test]
fn php_ini_in_the_working_directory_is_ignored() {
    let srv = harness::spawn_in_cwd("ini/precision.php", 1, "precision = 5\n");
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
    let srv = harness::spawn_with_phprc_and_config("ini/precision.php", 1, "precision = 5\n", "");
    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("precision=5"),
        "PHPRC must apply (got {body:?})\n{}",
        harness::diagnostics(&srv)
    );
}
