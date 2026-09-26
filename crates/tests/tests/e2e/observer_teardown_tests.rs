use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{drain, fixture, req};

use crate::harness::Spawn;

/// Teardown must close observer frames the save-handler bailout longjmp skipped, or the cycle-end walk hits freed VM-stack slots.
#[test]
fn bailing_save_handler_leaves_no_dangling_observer_frame() -> anyhow::Result<()> {
    let ini = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/observer_teardown_tests/observer-quiet.ini"
    ))?;
    let srv = Spawn::http(Mode::Worker, fixture("shared/session-bailout-worker.php"))
        .php_ini(&ini)
        .spawn();

    for _ in 0..3 {
        let (_, body) = drain(submit(srv.addr, req("/"))?);
        assert!(
            body.contains("sid="),
            "worker must keep serving (got {body:?})"
        );
    }
    Ok(())
}
