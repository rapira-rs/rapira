use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{drain, drain_resp, fixture, req, server_log};

use crate::harness::Spawn;

/// The mode is fixed for the process: each job over the resident loop gets the same Worker case.
#[test]
fn worker_mode_answers_worker_for_every_job() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("mode/worker.php")).spawn();
    for job in 0..2 {
        let resp = drain_resp(submit(srv.addr, req("/"))?);
        assert_eq!(resp.status(), 200, "job {job}");
        assert_eq!(resp.body_string(), "Worker:case:same:unbacked", "job {job}");
    }
    Ok(())
}

/// Dispatcher mode has no response to write into, so the answer travels out through the app log.
#[test]
fn dispatcher_mode_answers_dispatcher() -> anyhow::Result<()> {
    let mut srv = Spawn::http(Mode::Dispatcher, fixture("mode/dispatcher.php"))
        .json_log()
        .spawn();
    // The worker runs the script once when it starts. The stop comes after the record, so the log holds every record.
    server_log::wait_app_record(&srv.log_file(), "mode");
    srv.stop();

    let records: Vec<(String, String)> = server_log::records(&srv.log_file())
        .into_iter()
        .filter(|c| c.target == "app")
        .map(|c| (c.message, c.context))
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
    let srv = Spawn::http(Mode::Classic, fixture("mode/classic.php")).spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);

    assert_eq!(status, 200, "the script must run clean (body: {body:?})");
    assert_eq!(body, "Classic:case:done");
    Ok(())
}
