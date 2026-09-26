use rapira_sapi::Mode;
use serde_json::json;
use tests::wire::submit;
use tests::{drain, fixture, req, server_log};

use crate::harness::Spawn;

/// Outside dispatcher mode the call throws `NoDispatcherError`, catchable by its own name and by its stock parent.
#[test]
fn get_dispatcher_outside_dispatcher_mode_throws() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Classic,
        fixture("dispatcher/not-in-dispatcher-mode.php"),
    )
    .spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);

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
    let mut srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/worker-singleton.php"))
        .json_log()
        .spawn();
    // The worker runs the script once when it starts. The stop comes after the record, so the log holds every record.
    server_log::wait_app_record(&srv.log_file(), "dispatcher");
    srv.stop();
    let ctx = server_log::dispatcher_record(&srv.log_file())?;
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
    let srv = Spawn::http(Mode::Classic, fixture("dispatcher/host-created-only.php")).spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);

    assert_eq!(status, 200, "the refusal must be caught (body: {body:?})");
    assert!(
        body.contains("blocked:") && body.contains("done"),
        "private ctor must refuse new: {body:?}"
    );
    Ok(())
}
