use std::path::Path;

use php_sys::{Mode, Rapira};
use tests::{drain, fixture, php_lock_with_ini, req};

// Observer API registers only at module startup, so these tests run in their own process with their own ini; without --enable-zend-test the observer stays disabled (https://github.com/php/php-src/pull/5857).
fn observer_lock() -> std::sync::MutexGuard<'static, ()> {
    php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/observer_tests/observer.ini"
    )))
}

#[test]
fn observer_frames_balanced_after_bailout() -> anyhow::Result<()> {
    let _guard = observer_lock();
    let r = Rapira::start(Mode::Worker(fixture("observer_tests/observer-bailout.php")))?;
    let h = r.handle();

    let (_, probe) = drain(tests::submit(
        &h,
        req("/?mode=ok", "observer_tests/observer-bailout.php"),
    )?);
    if probe.contains("skip") {
        drop(h);
        drop(r);
        return Ok(());
    }

    let (_, b1) = drain(tests::submit(
        &h,
        req("/?mode=fatal", "observer_tests/observer-bailout.php"),
    )?);
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

    let (_, b2) = drain(tests::submit(
        &h,
        req("/?mode=ok", "observer_tests/observer-bailout.php"),
    )?);
    assert!(
        b2.contains("</outer>") && b2.contains("ok"),
        "worker survives, next request balanced (got {b2:?})"
    );

    drop(h);
    drop(r);
    Ok(())
}
