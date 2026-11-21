use rapira_sapi::plugin::Mode;
use serde_json::{Value, json};
use tests::wire::submit;
use tests::{StatusBody, drain, fixture, req};

use crate::harness::Spawn;

/// An env pair of the pool wins over an inherited variable of the same name. The `$_SERVER` of each request gets the pairs, and a request variable with the same name wins. A change to a pair in one request does not reach the next request of the worker.
#[test]
fn pool_env_reaches_the_worker() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        mode: Mode,
        fixture: &'static str,
        want: Value,
    }
    let cases = [
        Case {
            name: "worker mode: a pair wins over an inherited variable",
            mode: Mode::Worker,
            fixture: "pool_env/worker.php",
            want: json!({
                "boot_getenv": "config",
                "boot_server": "config",
                "boot_env": "config",
                "getenv": "config",
                "getenv_empty": "",
                "getenv_software": "config",
                "server": "config",
                "server_empty": "",
                "server_inherited": null,
                "server_software": "Rapira",
            }),
        },
        Case {
            name: "classic mode: a pair wins over an inherited variable",
            mode: Mode::Classic,
            fixture: "pool_env/classic.php",
            want: json!({
                "getenv": "config",
                "getenv_empty": "",
                "getenv_software": "config",
                "server": "config",
                "server_empty": "",
                "server_inherited": null,
                "server_software": "Rapira",
            }),
        },
    ];

    for case in &cases {
        let srv = Spawn::http(case.mode, fixture(case.fixture))
            .env("POOL_PROBE", "inherited")
            .env("POOL_INHERITED", "inherited")
            .toml("[http.pool.env]\nPOOL_PROBE = \"config\"\nPOOL_EMPTY = \"\"\nSERVER_SOFTWARE = \"config\"")
            .spawn();
        for n in 1..=3 {
            let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);
            assert_eq!(status, 200, "{} request {n}: {body}", case.name);
            let got: Value = serde_json::from_str(&body)?;
            assert_eq!(got, case.want, "{} request {n}", case.name);
        }
    }
    Ok(())
}
