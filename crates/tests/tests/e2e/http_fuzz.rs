//! Requests that Schemathesis generates from `fixtures/schemathesis/openapi.yaml`, through the HTTP listener in dispatcher mode, with `dispatcher/echo-json-worker.php` as the application: it replies with the Request as JSON.
//!
//! The operations send multipart bodies with fixed, optional, array, binary, object and free-form parts, an urlencoded form, raw bodies, path, query and header parameters. The check `rapira_echo` in `fixtures/schemathesis/hooks.py` compares each echo with the sent request: the method, target, authority, URI, protocol, header fields, the raw body, and each multipart field and file in document order. The expected values come from the PHP contract (`src/Http`) and RFC 9110 and RFC 9112. The built-in checks find a 5xx status and a status, content type or body that the spec does not document. After the run the worker processes are the same.
//!
//! Known exclusions:
//! - A backslash in a part name or filename: rapira reads it as a quoted-pair, <https://github.com/rapira-rs/rapira/issues/165>.
//! - A control byte in a part name or filename: rapira answers 400, <https://github.com/rapira-rs/rapira/issues/181>.
//! - The case of a header field name: rapira lowercases it on HTTP/1.1, so the check compares names without case, <https://github.com/rapira-rs/rapira/issues/180>.
//!
//! Schemathesis also sends negative cases: values that break the schema on purpose. The check accepts a 400 for a multipart body with an empty part name, a control byte or a backslash in a part name or filename, or a multipart body outside the urllib3 shape. It accepts a 413 for more files than the default `max_files`. It does not compare the parts of a body with a backslash or a body outside the urllib3 shape.
//!
//! Limits of the generation: a part filename is the part name with an optional extension, the CRLF framing is well-formed, the part headers are only `Content-Disposition` and `Content-Type`, and the header names are valid tokens. Each header field name appears once in a request and in a part, so the test does not check repeated values or their order. No request has a malformed boundary or request line. The positive cases stay below the default upload limits, so they get no 413. `dispatcher_loop::multipart_parse_outcomes_leave_no_spool_file`, `dispatcher_loop::rejected_bodies_never_reach_php`, `lifecycle::dispatcher_multipart_over_the_wire` and `lifecycle::chunked_body_over_the_cap_answers_413` test the 413 limits.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rapira_sapi::Mode;
use tests::fixture;

use crate::harness::{BOOT, Spawn, diagnostics, fixture_path, http_get, worker_pids};

/// Schemathesis 4.29.0. Dependabot does not track this image: bump the digest by hand.
const IMAGE: &str = "schemathesis/schemathesis@sha256:5586e94460479271714d47613175b3620c44b38b3e1babc936636108f1672a9e";

const CHECKS: &str = "not_a_server_error,status_code_conformance,content_type_conformance,response_schema_conformance,rapira_echo";

/// Runs Schemathesis with `seed` and `count` examples per operation against the echo worker. A missing `docker` binary fails the test.
fn fuzz(seed: u64, count: u32) {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/echo-json-worker.php")).spawn();
    // The first request waits for the worker boot, so the pid list holds the booted worker.
    let (status, body) = http_get(srv.addr, "/", BOOT)
        .unwrap_or_else(|e| panic!("GET /: {e}\n{}", diagnostics(&srv)));
    assert_eq!(
        status,
        200,
        "{}\n{}",
        String::from_utf8_lossy(&body),
        diagnostics(&srv)
    );
    let pids = worker_pids(srv.pid());

    // The working dir of Schemathesis: it writes its `.hypothesis` and `.schemathesis` dirs there.
    let out = srv.dir.join("schemathesis");
    std::fs::create_dir(&out).expect("create the Schemathesis output dir");
    // SAFETY: getuid and getgid always succeed.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let user = format!("{uid}:{gid}");
    let spec = fixture_path("schemathesis/openapi.yaml");
    let spec = format!("{}:/spec/openapi.yaml:ro", spec.display());
    let hooks = fixture_path("schemathesis/hooks.py");
    let hooks = format!("{}:/app/hooks.py:ro", hooks.display());
    let out = format!("{}:/out", out.display());
    let url = format!("http://{}", srv.addr);
    let (seed_arg, count_arg) = (seed.to_string(), count.to_string());
    let run = Command::new("docker")
        .args(["run", "--rm", "--network", "host", "--user", &user])
        .args(["-v", &spec, "-v", &hooks, "-v", &out, "-w", "/out", IMAGE])
        .args(["run", "/spec/openapi.yaml", "--url", &url])
        .args(["--seed", &seed_arg, "-n", &count_arg])
        .args(["--phases", "coverage,fuzzing", "--workers", "1"])
        .args(["--generation-database", "none"])
        // Above the keepalive and write timeouts of rapira: a lost response fails the run.
        .args(["--request-timeout", "90"])
        .args(["--checks", CHECKS, "--no-color"])
        .output()
        .unwrap_or_else(|e| panic!("run docker: {e}"));
    assert!(
        run.status.success(),
        "Schemathesis exited with {} for seed {seed}\n{}\n{}\n{}",
        run.status,
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
        diagnostics(&srv)
    );
    assert_eq!(worker_pids(srv.pid()), pids, "{}", diagnostics(&srv));
}

#[test]
fn generated_requests_cross_the_http_listener() {
    fuzz(1, 100);
}

/// A new seed on each run. The test prints the seed, so `fuzz(seed, 2000)` replays a failure.
#[test]
#[ignore = "a long run for the scheduled CI job"]
fn generated_requests_cross_the_http_listener_long() {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs();
    eprintln!("seed {seed}");
    fuzz(seed, 2000);
}
