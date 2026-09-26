use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{drain, fixture, req};

use crate::harness::Spawn;

/// A removed ini directive fails the module startup, and the master exits 1. A boot of the same script with a valid ini serves.
#[test]
fn removed_ini_directive_fails_the_module_startup() -> anyhow::Result<()> {
    let removed = std::fs::read_to_string(fixture("ini/failboot_tests/php-removed.ini"))?;
    let (status, log) = Spawn::http(Mode::Classic, fixture("shared/hello.php"))
        .php_ini(&removed)
        .boot_failure();
    assert_eq!(
        status.code(),
        Some(1),
        "removed-directive ini must fail startup\n{log}"
    );
    assert!(log.contains("php_module_startup failed"), "\n{log}");

    let srv = Spawn::http(Mode::Classic, fixture("shared/hello.php")).spawn();
    assert_eq!(drain(submit(srv.addr, req("/"))?).0, 200);
    Ok(())
}
