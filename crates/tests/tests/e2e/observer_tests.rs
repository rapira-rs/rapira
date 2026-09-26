use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{drain, fixture, req};

use crate::harness::Spawn;

// Observer API registers only at module startup, so these tests boot with their own ini; without --enable-zend-test the observer stays disabled (https://github.com/php/php-src/pull/5857).
const OBSERVER_INI: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/ini/observer_tests/observer.ini"
);

#[test]
fn observer_frames_balanced_after_bailout() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("observer_tests/observer-bailout.php"))
        .php_ini(&std::fs::read_to_string(OBSERVER_INI)?)
        .spawn();

    let (_, probe) = drain(submit(srv.addr, req("/?mode=ok"))?);
    if probe.contains("skip") {
        return Ok(());
    }

    let (_, b1) = drain(submit(srv.addr, req("/?mode=fatal"))?);
    let mut pos = 0;
    for marker in ["<outer>", "<inner>", "</inner>", "</outer>"] {
        let i = b1[pos..].find(marker).unwrap_or_else(|| {
            panic!("observer markers out of order or missing: expected {marker} (got {b1:?})")
        });
        pos += i + marker.len();
    }
    for close in ["</inner>", "</outer>"] {
        assert_eq!(
            b1.matches(close).count(),
            1,
            "each frame must close exactly once (got {b1:?})"
        );
    }

    let (_, b2) = drain(submit(srv.addr, req("/?mode=ok"))?);
    assert!(
        b2.contains("</outer>") && b2.contains("ok"),
        "worker survives, next request balanced (got {b2:?})"
    );
    Ok(())
}
