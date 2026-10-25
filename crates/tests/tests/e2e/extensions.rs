use std::collections::HashMap;
use std::time::Duration;

use tests::StatusBody;

use crate::harness::{
    Server, diagnostics, fan_out, fixture_path, http_get, http_get_with_headers, spawn_with_config,
    spawn_with_phprc_and_config, wait_workers,
};

const REQ: Duration = Duration::from_secs(10);

/// One answer of an `*-rng-worker.php` fixture.
struct RngDraw {
    pid: u32,
    /// The first draw of the worker, as hex.
    first: String,
}

fn rng_draw(body: &str) -> RngDraw {
    let (pid, first) = body
        .strip_prefix("pid=")
        .and_then(|r| r.split_once(" first="))
        .unwrap_or_else(|| panic!("unexpected body {body:?}"));
    let pid: u32 = pid
        .parse()
        .unwrap_or_else(|_| panic!("bad pid in {body:?}"));
    RngDraw {
        pid,
        first: first.to_owned(),
    }
}

/// No two workers may emit identical first draws; the cached draw also pins that the entrypoint top level runs once per worker.
fn assert_distinct_first_draws(fixture: &str) {
    let srv = spawn_with_config(fixture, 4, "");
    let workers = wait_workers(&srv, Duration::from_secs(20), "4 workers", |p| p.len() == 4);
    let StatusBody { body, .. } = http_get(srv.addr, "/", REQ).expect("GET /");
    if body == b"skip" {
        return;
    }

    let mut per_pid: HashMap<u32, String> = HashMap::new();
    for _ in 0..5 {
        for RngDraw { pid, first } in fan_out(srv.addr, "/", 4, 10, rng_draw) {
            assert!(
                first.len() == 32
                    && first
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "bin2hex of 16 bytes must be 32 lowercase hex chars (got {first:?})"
            );
            match per_pid.get(&pid) {
                Some(prev) => assert_eq!(
                    prev, &first,
                    "a worker's cached first draw must not change (pid {pid})"
                ),
                None => {
                    per_pid.insert(pid, first);
                }
            }
        }
        if per_pid.len() == workers.len() {
            break;
        }
    }
    // accept scheduling may starve a worker (seen on macOS); two distinct pids prove the claim
    assert!(
        per_pid.len() >= 2,
        "at least two workers must answer\n{}",
        diagnostics(&srv)
    );
    let mut firsts: Vec<&String> = per_pid.values().collect();
    firsts.sort_unstable();
    firsts.dedup();
    assert_eq!(
        firsts.len(),
        per_pid.len(),
        "distinct workers must draw distinct first bytes (per pid: {per_pid:?})"
    );
}

/// openssl_random_pseudo_bytes() draws from the OpenSSL DRBG; random_bytes() never uses OpenSSL.
#[test]
fn forked_workers_do_not_share_the_openssl_drbg() {
    assert_distinct_first_draws("extensions/openssl-rng-worker.php");
}

/// MINIT calls sodium_init() in the master before the fork; each worker must still draw its own randombytes_buf() bytes.
#[test]
fn forked_workers_do_not_share_the_sodium_rng() {
    assert_distinct_first_draws("extensions/sodium-rng-worker.php");
}

fn browscap_ini() -> String {
    format!(
        "browscap = \"{}\"\ndisplay_errors = On\nerror_reporting = E_ALL\nmax_execution_time = 0\nsession.gc_probability = 0\n",
        fixture_path("browscap/rapira-browscap.ini").display()
    )
}

fn browscap_probe(srv: &Server, ua: &str) -> String {
    let StatusBody { status: code, body } =
        http_get_with_headers(srv.addr, "/", &[("User-Agent", ua)], REQ).expect("GET /");
    assert_eq!(code, 200, "UA {ua:?}\n{}", diagnostics(srv));
    String::from_utf8_lossy(&body).into_owned()
}

/// The `browscap` ini is PHP_INI_SYSTEM: MINIT parses the file once into persistent memory, in the master before the fork.
#[test]
fn get_browser_uses_browscap_and_isolates_user_agent_requests() {
    let srv = spawn_with_phprc_and_config(
        "browscap/browscap-worker.php",
        1,
        &browscap_ini(),
        "mode = \"worker\"\n",
    );
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

    struct Case {
        ua: &'static str,
        expected: &'static str,
    }
    let cases = [
        Case {
            ua: "Rapira/1.0 (Linux x86_64) Bot/9",
            expected: "browser=RapiraBot\n\
             platform=unknown\n\
             crawler=1\n\
             version=1.0\n\
             parent=RapiraBase\n\
             browser_name_pattern=Rapira/1.0 (Linux*) Bot*\n\
             browser_name_regex=~^rapira/1\\.0 \\(linux.*\\) bot.*$~",
        },
        Case {
            ua: "Rapira/1.0 (Darwin)",
            expected: "browser=RapiraProbe\n\
             platform=unknown\n\
             crawler=\n\
             version=1.0\n\
             parent=RapiraBase\n\
             browser_name_pattern=Rapira/1.0*\n\
             browser_name_regex=~^rapira/1\\.0.*$~",
        },
        Case {
            ua: "curl/8.5.0",
            expected: "browser=RapiraDefault\n\
             platform=unknown\n\
             crawler=\n\
             version=0\n\
             parent=<unset>\n\
             browser_name_pattern=Default Browser Capability Settings\n\
             browser_name_regex=~^default browser capability settings$~",
        },
    ];
    for Case { ua, expected } in cases {
        assert_eq!(browscap_probe(&srv, ua), expected, "UA {ua:?}");
    }

    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("GET /");
    let body = String::from_utf8_lossy(&body);
    assert_eq!(code, 200, "{}", diagnostics(&srv));
    assert!(
        body.contains("HTTP_USER_AGENT") && body.contains("browscap:false"),
        "expected the warning and a false return (got {body:?})"
    );
}

/// One answer of browscap-worker.php to `?probe=pid`.
struct BrowscapReply {
    table: String,
    pid: u32,
}

fn table_and_pid(body: &str) -> BrowscapReply {
    let (table, pid) = body
        .rsplit_once("\npid=")
        .unwrap_or_else(|| panic!("no pid line in {body:?}"));
    BrowscapReply {
        table: table.to_owned(),
        pid: pid
            .parse()
            .unwrap_or_else(|_| panic!("bad pid in {body:?}")),
    }
}

/// One MINIT parse in the master serves every worker: identical answers from distinct pids, request after request.
#[test]
fn browscap_table_is_shared_and_stable_across_workers() {
    let srv = spawn_with_phprc_and_config(
        "browscap/browscap-worker.php",
        3,
        &browscap_ini(),
        "mode = \"worker\"\n",
    );
    let workers = wait_workers(&srv, Duration::from_secs(20), "3 workers", |p| p.len() == 3);
    let mut tables: Vec<String> = Vec::new();
    let mut pids: Vec<u32> = Vec::new();
    for _ in 0..5 {
        for BrowscapReply { table, pid } in fan_out(srv.addr, "/?probe=pid", 4, 10, table_and_pid) {
            tables.push(table);
            pids.push(pid);
        }
        pids.sort_unstable();
        pids.dedup();
        if pids.len() == workers.len() {
            break;
        }
    }
    // accept scheduling may starve a worker (seen on macOS); two distinct pids prove the claim
    assert!(
        pids.len() >= 2,
        "at least two workers must answer\n{}",
        diagnostics(&srv)
    );
    tables.sort();
    tables.dedup();
    assert_eq!(
        tables.len(),
        1,
        "every worker must serve the identical table (got {} variants)",
        tables.len()
    );
}
