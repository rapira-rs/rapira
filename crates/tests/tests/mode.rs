use php_sys::{Mode, Rapira};
use tests::{captured, drain, drain_resp, fixture, init_log_capture, php_lock, req};

/// The mode is fixed for the process: each job over the resident loop gets the same Worker case.
#[test]
fn worker_mode_answers_worker_for_every_job() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("mode/worker.php")))?;
    let h = r.handle();
    for job in 0..2 {
        let resp = drain_resp(tests::submit(&h, req("/", "mode/worker.php"))?);
        assert_eq!(resp.status(), 200, "job {job}");
        assert_eq!(resp.body_string(), "Worker:case:same:unbacked", "job {job}");
    }
    drop(h);
    drop(r);
    Ok(())
}

/// Dispatcher mode has no response to write into, so the answer travels out through the app log.
#[test]
fn dispatcher_mode_answers_dispatcher() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture("mode/dispatcher.php")))?;
    drop(r);

    let records: Vec<(String, String)> = captured()
        .iter()
        .filter(|c| c.target == "app")
        .map(|c| (c.message.clone(), c.context.clone()))
        .collect();
    assert_eq!(records.len(), 1, "one mode record (got {records:?})");
    let (msg, ctx) = &records[0];
    assert_eq!(msg, "mode");
    for fragment in [
        r#""name":"Dispatcher""#,
        r#""case":true"#,
        r#""class":"Rapira\\Mode""#,
    ] {
        assert!(ctx.contains(fragment), "missing {fragment} in {ctx:?}");
    }
    Ok(())
}

/// Classic mode answers as well. No other mode can cover this: the answer is the mode itself.
#[test]
fn classic_mode_answers_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(tests::submit(&h, req("/", "mode/classic.php"))?);
    drop(h);
    drop(r);

    assert_eq!(status, 200, "the script must run clean (body: {body:?})");
    assert_eq!(body, "Classic:case:done");
    Ok(())
}
