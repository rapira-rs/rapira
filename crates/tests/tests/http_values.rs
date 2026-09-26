use rapira_sapi::{Mode, Rapira};
use tests::{drain, php_lock, req};

/// Pins that Http value objects construct and refuse readonly reassignment, wrong arity, and a bad address union.
#[test]
fn value_objects_construct_and_refuse() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(tests::submit(&h, req("/", "http_values/construct.php"))?);
    drop(h);
    drop(r);

    assert_eq!(status, 200, "construction must succeed (body: {body:?})");
    for line in [
        "tls-class: Rapira\\Tls",
        "POST /upload?x=1 HTTP/2",
        "203.0.113.7:44123",
        "server-path=NULL",
        "note=hello",
        "me.png 512",
        "h2 NULL",
        "example.test:8443 1722700000.25",
        "tls-null: NULL",
        "readonly: enforced",
        "arity: enforced",
        "union: enforced",
        "done",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}
