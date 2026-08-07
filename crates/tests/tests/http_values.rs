//! The `Rapira\Http` value objects: constructors, readonly, type refusals.

use php_sys::{Mode, Rapira};
use tests::{drain, php_lock, req};

/// Builds the full object graph up through an 11-argument Request, reads every
/// tier back, then probes the three refusals: readonly reassignment, wrong
/// arity (which is what proves the constructors exist), and the address union.
#[test]
fn value_objects_construct_and_refuse() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "http_values/construct.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "construction must succeed (body: {body:?})");
    for line in [
        "POST /upload?x=1 HTTP/2",
        "203.0.113.7:44123",
        "NULL",
        "note=hello",
        "me.png 512",
        "h2 NULL",
        "example.test:8443 1722700000.25",
        "readonly: enforced",
        "arity: enforced",
        "union: enforced",
        "done",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}
