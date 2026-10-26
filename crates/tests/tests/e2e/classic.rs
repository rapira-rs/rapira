use crate::harness::{self, Spawn, http_get, slot_line};
use rapira_sapi::plugin::Mode;
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
    slot_line(&srv, &format!("handled {} errors 0 ", cases.len()));
    Ok(())
}

/// A worker enters the entrypoint directory once and stays there, so a userland chdir outlives the request that made it.
#[test]
fn classic_mode_keeps_the_working_directory_across_requests() {
    let srv = harness::spawn_with_config("classic/cwd.php", 1, "mode = \"classic\"\n");
    // getcwd() reports the resolved path, and the scratch dir can sit under a symlinked temp dir.
    let entrypoint_dir = std::fs::canonicalize(&srv.dir).expect("canonicalize the scratch dir");
    let entrypoint_dir = entrypoint_dir.to_string_lossy().into_owned();

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
    let moved = String::from_utf8_lossy(&body).into_owned();
    assert_ne!(
        moved,
        entrypoint_dir,
        "the fixture must chdir out of the entrypoint directory\n{}",
        harness::diagnostics(&srv)
    );

    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        moved,
        "the chdir of the previous request must still hold\n{}",
        harness::diagnostics(&srv)
    );
}
