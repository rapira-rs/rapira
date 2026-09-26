use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{assert_skip_allowed, drain, fixture, req, server_log};

use crate::harness::Spawn;

// ext/imap (PECL imap) reads default_socket_timeout at module startup only; the ini value 17 differs from the built-in 60, so the snapshot is provable.
const IMAP_INI: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/ini/imap_tests/imap.ini"
);

fn run(name: &str, uris: &[&str]) -> anyhow::Result<Vec<(u16, String)>> {
    let srv = Spawn::http(Mode::Worker, fixture(name))
        .php_ini(&std::fs::read_to_string(IMAP_INI)?)
        .spawn();
    uris.iter()
        .map(|uri| Ok(drain(submit(srv.addr, req(uri))?)))
        .collect()
}

/// MINIT stores FG(default_socket_timeout) into c-client once (PECL imap php_imap.c, SET_*TIMEOUT): no request can change the imap socket timeouts.
#[test]
fn imap_timeout_is_snapshotted_at_minit() -> anyhow::Result<()> {
    let out = run("imap_tests/imap-timeout-worker.php", &["/", "/"])?;
    if out[0].1 == "skip" {
        assert_skip_allowed("imap_tests/imap-timeout-worker.php");
        return Ok(());
    }
    let expected = "imap:open=17:after_ini_set=17:read=17:ini=5";
    assert_eq!(
        (out[0].0, out[0].1.as_str()),
        (200, expected),
        "c-client must keep the MINIT snapshot after ini_set (got: {:?})",
        out[0]
    );
    assert_eq!(
        (out[1].0, out[1].1.as_str()),
        (200, expected),
        "RINIT does not re-read the ini, so the next request sees the same snapshot (got: {:?})",
        out[1]
    );
    Ok(())
}

/// The per-job RSHUTDOWN reports each undrained entry as an E_NOTICE through the SAPI log_message hook; the log is read after the stop, so it holds every record of the worker.
#[test]
fn imap_undrained_error_reaches_the_log() -> anyhow::Result<()> {
    let name = "imap_tests/imap-errors-worker.php";
    let mut srv = Spawn::http(Mode::Worker, fixture(name))
        .php_ini(&std::fs::read_to_string(IMAP_INI)?)
        .json_log()
        .spawn();

    let (s1, b1) = drain(submit(srv.addr, req("/?step=leak"))?);
    if b1 == "skip" {
        assert_skip_allowed(name);
        return Ok(());
    }
    assert_eq!(
        (s1, b1.as_str()),
        (200, "imap:leaked:1"),
        "the leak request must push one entry and keep it undrained"
    );
    let (s2, b2) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(
        (s2, b2.as_str()),
        (200, "imap:errors:empty"),
        "the follow-up must see a reset stack"
    );
    srv.stop();

    let seen = server_log::records(&srv.log_file())
        .iter()
        .any(|c| c.target == "php" && c.message.contains("invalid remote specification (errflg="));
    assert!(
        seen,
        "the RSHUTDOWN notice for the undrained error must reach the php log target"
    );
    Ok(())
}
