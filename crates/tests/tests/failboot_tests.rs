use std::path::Path;

use php_sys::{Mode, Rapira};
use tests::{drain, php_lock, req, set_phprc};

/// Pins that a start after a module-startup failure still runs a full module startup, not the early-return path.
#[test]
fn module_startup_failure_then_clean_restart() -> anyhow::Result<()> {
    let php = php_lock();
    set_phprc(
        &php,
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/ini/failboot_tests/php-removed.ini"
        )),
    );
    assert!(
        Rapira::start(Mode::Classic).is_err(),
        "removed-directive ini must fail startup"
    );

    set_phprc(
        &php,
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/ini/shared/php.ini"
        )),
    );
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    assert_eq!(
        drain(tests::submit(&h, req("/", "shared/hello.php"))?).0,
        200
    );
    drop(h);
    r.shutdown();
    Ok(())
}
