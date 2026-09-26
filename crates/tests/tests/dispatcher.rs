use rapira_sapi::{Mode, Rapira};
use serde_json::json;
use tests::{dispatcher_record, drain, fixture, php_lock, req};

/// Outside dispatcher mode the call throws `NoDispatcherError`, catchable by its own name and by its stock parent.
#[test]
fn get_dispatcher_outside_dispatcher_mode_throws() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic, None)?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(
        &h,
        req("/", "dispatcher/not-in-dispatcher-mode.php"),
    )?);
    drop(h);
    drop(r);

    assert_eq!(
        status, 200,
        "every throw in the script must be caught (body: {body:?})"
    );
    for line in [
        "class: Rapira\\Exception\\NoDispatcherError",
        "rapira: yes",
        "timeout-as-runtime: yes",
        "done",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}

/// Dispatcher singleton identity, interface chain, and blocked clone, reported through the app log.
#[test]
fn worker_singleton() -> anyhow::Result<()> {
    let _guard = php_lock();
    let ctx = dispatcher_record(|| {
        Rapira::start(
            Mode::Dispatcher(fixture("dispatcher/worker-singleton.php")),
            Some(rapira_sapi::http::DISPATCHER_CLASSES),
        )
    })?;
    for (key, want) in [
        ("class", json!("Rapira\\Internal\\Http\\Dispatcher")),
        ("name", json!("http")),
        ("same", json!(true)),
        ("http", json!(true)),
        ("base", json!(true)),
        ("clone", json!("blocked")),
    ] {
        assert_eq!(ctx[key], want, "{key} in {ctx}");
    }
    Ok(())
}

/// `new` on the Internal classes must be refused by the private constructor.
#[test]
fn host_created_only() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic, None)?;
    let h = r.sink();
    let (status, body) = drain(tests::submit(
        &h,
        req("/", "dispatcher/host-created-only.php"),
    )?);
    drop(h);
    drop(r);

    assert_eq!(status, 200, "the refusal must be caught (body: {body:?})");
    assert!(
        body.contains("blocked:") && body.contains("done"),
        "private ctor must refuse new: {body:?}"
    );
    Ok(())
}
