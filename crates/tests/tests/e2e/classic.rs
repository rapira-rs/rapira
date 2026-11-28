use crate::harness::{self, Spawn, http_get, slot_line};
use rapira_config::Mode;
use std::time::Duration;
use tests::wire::submit;
use tests::{StatusBody, drain_resp, req};

const REQ: Duration = Duration::from_secs(10);

struct CallbackExit {
    name: &'static str,
    query: &'static str,
    status: u16,
    body: &'static str,
}

/// exit() in a callback that PHP calls with no frame on the stack ends the reply complete, as `php -S` does.
#[test]
fn exit_in_a_frameless_callback_ends_the_reply_complete() -> anyhow::Result<()> {
    let cases = [
        CallbackExit {
            name: "exception handler after output",
            query: "/?exit=handler",
            status: 404,
            body: "Not Found\n",
        },
        CallbackExit {
            name: "shutdown function after output",
            query: "/?exit=shutdown",
            status: 200,
            body: "ok\nbye\n",
        },
    ];
    let srv = Spawn::http(
        Mode::Classic,
        harness::fixture_path("classic/exit-in-callback.php"),
    )
    .spawn();
    for case in &cases {
        let resp = drain_resp(submit(srv.addr, req(case.query))?);
        assert_eq!(resp.status(), case.status, "{}: status", case.name);
        assert_eq!(resp.body_string(), case.body, "{}: body", case.name);
        assert!(
            resp.ended && !resp.truncated,
            "{}: the reply must end whole (ended={}, truncated={})",
            case.name,
            resp.ended,
            resp.truncated
        );
    }
    // exit() is a normal end, so the slot counts no error.
    slot_line(&srv, &format!("handled={} errors=0 ", cases.len()));
    Ok(())
}

/// A fatal in the header callback of a request without output ends that request, and the same worker serves the next request, as `php -S` does.
#[test]
fn header_callback_fatal_keeps_the_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(
        Mode::Classic,
        harness::fixture_path("classic/header-callback-fatal.php"),
    )
    .spawn();
    let before = drain_resp(submit(srv.addr, req("/"))?);
    drain_resp(submit(srv.addr, req("/?fatal=1"))?);
    let after = drain_resp(submit(srv.addr, req("/"))?);

    assert_eq!(
        (before.status(), after.status()),
        (200, 200),
        "{}",
        harness::diagnostics(&srv)
    );
    assert_eq!(before.body_string(), "ok");
    assert_eq!(after.body_string(), "ok");
    slot_line(&srv, "handled=3 ");
    Ok(())
}

/// Each classic request starts in the entrypoint directory, as `php_execute_script` enters it. A chdir() by the app ends with the request that made it.
#[test]
fn classic_mode_starts_each_request_in_the_entrypoint_directory() {
    let srv = Spawn::http(Mode::Classic, harness::fixture_path("classic/cwd.php")).spawn();
    // getcwd() reports the resolved path, and the scratch directory can be under a symlinked temp directory.
    let entrypoint_dir =
        std::fs::canonicalize(srv.dir.join("http")).expect("canonicalize the entrypoint dir");
    let entrypoint_dir = entrypoint_dir.to_string_lossy().into_owned();
    // sys_get_temp_dir() reads TMPDIR, which the server inherits from the test.
    let temp_dir = std::fs::canonicalize(std::env::temp_dir()).expect("canonicalize the temp dir");
    let temp_dir = temp_dir.to_string_lossy().into_owned();

    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        entrypoint_dir,
        "the first request must run in the entrypoint directory\n{}",
        harness::diagnostics(&srv)
    );

    let StatusBody { status: code, body } = http_get(srv.addr, "/chdir", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        temp_dir,
        "the chdir() of the request must take effect in that request\n{}",
        harness::diagnostics(&srv)
    );

    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        entrypoint_dir,
        "the next request must start in the entrypoint directory again\n{}",
        harness::diagnostics(&srv)
    );
}
