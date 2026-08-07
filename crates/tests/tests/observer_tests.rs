use std::path::Path;

use php_sys::{Mode, Rapira};
use tests::{drain, fixture, php_lock_with_ini, req};

// The observer API only registers at module startup, and zend_test's observer writes
// markers into the response body - so these run in their own process with their own ini,
// never the shared suite. Requires PHP built with --enable-zend-test; without it the
// observer API stays disabled and both tests degrade to plain worker runs.
// https://github.com/php/php-src/pull/5857
fn observer_lock() -> std::sync::MutexGuard<'static, ()> {
    php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/observer_tests/observer.ini"
    )))
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn observer_frames_balanced_after_bailout() -> anyhow::Result<()> {
    let _guard = observer_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "observer_tests/observer-bailout.php",
    )))?;
    let h = r.handle()?;

    let (_, probe) =
        drain(h.handle_blocking(req("/?mode=ok", "observer_tests/observer-bailout.php"))?);
    if probe.contains("skip") {
        drop(h);
        r.shutdown();
        return Ok(()); // PHP built without --enable-zend-test
    }

    // outer() -> inner() -> trigger_error(E_USER_ERROR) bails; the closing tags
    // for both frames open at the bailout must still reach the response body,
    // properly nested and exactly once — an out-of-order or duplicated close is
    // precisely the unbalanced-observer-frames regression this test guards.
    let (_, b1) =
        drain(h.handle_blocking(req("/?mode=fatal", "observer_tests/observer-bailout.php"))?);
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

    // worker survives; the next request's frames are balanced too
    let (_, b2) =
        drain(h.handle_blocking(req("/?mode=ok", "observer_tests/observer-bailout.php"))?);
    assert!(
        b2.contains("</outer>") && b2.contains("ok"),
        "worker survives, next request balanced (got {b2:?})"
    );

    drop(h);
    r.shutdown();
    Ok(())
}
