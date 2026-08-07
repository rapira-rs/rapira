use std::path::Path;

use php_sys::{Mode, Rapira};
use tests::{drain, fixture, php_lock_with_ini, req};

// A bailing save handler bails inside rapira_reset_session. The longjmp skips the observer
// end handlers of every frame it abandons; unless rapira_request_teardown closes them,
// EG(current_observed_frame) keeps pointing at VM-stack slots the PHP worker loop frees the
// moment HttpHandler::handleRequest returns, and zend_observer_fcall_end_all() walks into them
// at cycle end. Pre-fix: SIGSEGV (release) / a spin inside end_all (debug).
//
// Own binary, own ini: PHPRC is process-global, and zend_test's markers must stay OFF here -
// printing them perturbs the arena enough to hide the fault. Needs --enable-zend-test;
// without it the observer API never registers and this degrades to a plain session test.
// https://github.com/php/php-src/pull/5857
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn bailing_save_handler_leaves_no_dangling_observer_frame() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/observer_teardown_tests/observer-quiet.ini"
    )));
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "shared/session-bailout-worker.php",
    )))?;
    let h = r.handle()?;

    // each job bails in teardown and recycles; the cycle end walks the observer chain
    for _ in 0..3 {
        let (_, body) = drain(h.handle_blocking(req("/", "shared/session-bailout-worker.php"))?);
        assert!(
            body.contains("sid="),
            "worker must keep serving (got {body:?})"
        );
    }

    drop(h);
    r.shutdown(); // must not fault
    Ok(())
}
