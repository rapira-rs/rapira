use crate::harness::{self, http_get};
use std::time::Duration;

const REQ: Duration = Duration::from_secs(10);

/// A worker enters the entrypoint directory once and stays there, so a userland chdir outlives the request that made it.
#[test]
fn classic_mode_keeps_the_working_directory_across_requests() {
    let srv = harness::spawn_with_config("classic/cwd.php", 1, "mode = \"classic\"\n");
    // getcwd() reports the resolved path, and the scratch dir can sit under a symlinked temp dir.
    let entrypoint_dir = std::fs::canonicalize(&srv.dir).expect("canonicalize the scratch dir");
    let entrypoint_dir = entrypoint_dir.to_string_lossy().into_owned();

    let (code, body) = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        entrypoint_dir,
        "the first request must run in the entrypoint directory\n{}",
        harness::diagnostics(&srv)
    );

    let (code, body) = http_get(srv.addr, "/chdir", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    let moved = String::from_utf8_lossy(&body).into_owned();
    assert_ne!(
        moved,
        entrypoint_dir,
        "the fixture must chdir out of the entrypoint directory\n{}",
        harness::diagnostics(&srv)
    );

    let (code, body) = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        moved,
        "the chdir of the previous request must still hold\n{}",
        harness::diagnostics(&srv)
    );
}
