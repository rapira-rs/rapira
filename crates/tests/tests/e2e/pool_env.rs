use rapira_sapi::plugin::Mode;
use serde_json::{Value, json};
use tests::wire::submit;
use tests::{StatusBody, drain, fixture, req};

use crate::harness::Spawn;

/// An env pair of the pool wins over an inherited variable of the same name.
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
            }),
        },
    ];

    for case in &cases {
        let srv = Spawn::http(case.mode, fixture(case.fixture))
            .env("POOL_PROBE", "inherited")
            .env("POOL_INHERITED", "inherited")
            .toml("[http.pool.env]\nPOOL_PROBE = \"config\"\nPOOL_EMPTY = \"\"\nSERVER_SOFTWARE = \"config\"")
            .spawn();
        let StatusBody { status, body } = drain(submit(srv.addr, req("/"))?);
        assert_eq!(status, 200, "{}: {body}", case.name);
        let got: Value = serde_json::from_str(&body)?;
        assert_eq!(got, case.want, "{}", case.name);
    }
    Ok(())
}
