use std::path::Path;

use rapira_sapi::{Mode, Rapira};
use tests::{drain, fixture, php_lock_with_ini, req};

/// Teardown must close observer frames the save-handler bailout longjmp skipped, or the cycle-end walk hits freed VM-stack slots.
#[test]
fn bailing_save_handler_leaves_no_dangling_observer_frame() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/observer_teardown_tests/observer-quiet.ini"
    )));
    let r = Rapira::start(
        Mode::Worker,
        fixture("shared/session-bailout-worker.php"),
        None,
    )?;
    let h = r.sink();

    for _ in 0..3 {
        let (_, body) = drain(tests::submit(&h, req("/"))?);
        assert!(
            body.contains("sid="),
            "worker must keep serving (got {body:?})"
        );
    }

    drop(h);
    drop(r);
    Ok(())
}
