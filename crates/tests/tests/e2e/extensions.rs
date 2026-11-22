use std::collections::BTreeSet;
use std::time::Duration;

use rapira_sapi::plugin::Mode;
use tests::StatusBody;

use crate::harness::{
    Server, Spawn, diagnostics, fan_out, fixture_path, http_get, http_get_with_headers,
    wait_serving,
};

const REQ: Duration = Duration::from_secs(10);

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

/// MINIT reads the `browscap` file once. Each request uses its own user agent.
#[test]
fn get_browser_uses_browscap_and_isolates_user_agent_requests() {
    let srv = Spawn::http(Mode::Worker, fixture_path("browscap/browscap-worker.php"))
        .php_ini(&browscap_ini())
        .spawn();
    wait_serving(&srv, "http", 1);

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

/// Each worker reads the same browser table across requests.
#[test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
fn browscap_table_is_shared_and_stable_across_workers() {
    const WORKERS: usize = 3;
    let srv = Spawn::http(Mode::Worker, fixture_path("browscap/browscap-worker.php"))
        .workers(WORKERS)
        .php_ini(&browscap_ini())
        .spawn();
    wait_serving(&srv, "http", WORKERS);
    let expected = browscap_probe(&srv, "Rapira/1.0 (Darwin)");
    let mut apps = BTreeSet::new();
    for _ in 0..5 {
        for body in fan_out(srv.addr, "/?probe=app", 4, 10, str::to_owned) {
            let (table, app) = body.rsplit_once("\napp=").expect("table and boot token");
            assert_eq!(table, expected, "browser table of app {app}");
            apps.insert(app.to_owned());
        }
        if apps.len() == WORKERS {
            break;
        }
    }
    assert_eq!(apps.len(), WORKERS, "serving apps: {apps:?}");
}
