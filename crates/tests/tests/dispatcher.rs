//! `\Rapira\get_dispatcher()` and the `Rapira\Exception` class set.

use php_sys::{Mode, Rapira};
use tests::{captured, drain, fixture, init_log_capture, php_lock, req};

/// Outside worker mode nothing feeds this process work, so the call must throw
/// the specific `NotInWorkerModeError` — catchable by its own name, branded
/// `RapiraThrowable` — and the `RuntimeException` family must be catchable by
/// its stock parent. Hierarchy is asserted through catch behavior: a wrong
/// parent CE passed to a registrar compiles fine and only fails here.
#[test]
fn get_dispatcher_outside_worker_mode_throws() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/not-in-worker-mode.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(
        status, 200,
        "every throw in the script must be caught (body: {body:?})"
    );
    for line in [
        "class: Rapira\\Exception\\NotInWorkerModeError",
        "rapira: yes",
        "timeout-as-runtime: yes",
        "done",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}

/// Singleton identity, the interface chain, and the clone block — reported by
/// the worker script through the app log, since worker output has nowhere else
/// to go until the Exchange verbs land.
#[test]
fn worker_singleton() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::WorkerRequest(fixture(
        "dispatcher/worker-singleton.php",
    )))?;
    r.shutdown(); // joins the worker; the script has run to completion

    let records: Vec<(String, String)> = captured()
        .iter()
        .filter(|c| c.target == "app")
        .map(|c| (c.message.clone(), c.context.clone()))
        .collect();
    assert_eq!(records.len(), 1, "one dispatcher record (got {records:?})");
    let (msg, ctx) = &records[0];
    assert_eq!(msg, "dispatcher");
    for fragment in [
        r#""class":"Rapira\\Internal\\Http\\Dispatcher""#,
        r#""name":"http""#,
        r#""same":true"#,
        r#""http":true"#,
        r#""base":true"#,
        r#""clone":"blocked""#,
    ] {
        assert!(ctx.contains(fragment), "missing {fragment} in {ctx:?}");
    }
    Ok(())
}

/// `new` on the Internal classes must be refused by the private constructor.
#[test]
fn host_created_only() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/host-created-only.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "the refusal must be caught (body: {body:?})");
    assert!(
        body.contains("blocked:") && body.contains("done"),
        "private ctor must refuse new: {body:?}"
    );
    Ok(())
}
