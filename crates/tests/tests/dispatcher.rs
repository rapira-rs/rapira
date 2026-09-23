use php_sys::{Mode, Rapira};
use tests::{captured, drain, fixture, init_log_capture, php_lock, req};

/// Outside dispatcher mode the call throws `NoDispatcherError`, catchable by its own name and by its stock parent.
#[test]
fn get_dispatcher_outside_dispatcher_mode_throws() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
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
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/worker-singleton.php")))?;
    drop(r);

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
    let h = r.handle();
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
