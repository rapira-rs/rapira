use rapira_sapi::Mode;
use serde_json::{Value, json};
use tests::wire::submit;
use tests::{drain, fixture, req};

use crate::harness::Spawn;

/// The boot run of the entrypoint gets the $_SERVER of `php entrypoint.php`: the process environment, then the entrypoint path, which wins over an environment variable of the same name.
/// https://github.com/php/php-src/blob/php-8.5.11/sapi/cli/php_cli.c#L316-L347
#[test]
fn boot_server_follows_the_cli() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        mode: Mode,
        fixture: &'static str,
        env: &'static [(&'static str, &'static str)],
        ini: &'static str,
    }
    let cases = [
        Case {
            name: "worker mode",
            mode: Mode::Worker,
            fixture: "boot_server/worker.php",
            env: &[("BOOT_PROBE", "from-env")],
            ini: "",
        },
        Case {
            name: "a $argv without a refcount set before the first $_SERVER read changes nothing",
            mode: Mode::Worker,
            fixture: "boot_server/worker-argv-null.php",
            env: &[("BOOT_PROBE", "from-env")],
            ini: "",
        },
        Case {
            name: "dispatcher mode",
            mode: Mode::Dispatcher,
            fixture: "boot_server/dispatcher.php",
            env: &[("BOOT_PROBE", "from-env")],
            ini: "",
        },
        Case {
            name: "variables_order without E still imports the environment into $_SERVER",
            mode: Mode::Dispatcher,
            fixture: "boot_server/dispatcher.php",
            env: &[("BOOT_PROBE", "from-env")],
            ini: "variables_order = \"GPCS\"",
        },
        Case {
            name: "register_argc_argv = Off still gives argv, as in the CLI",
            mode: Mode::Dispatcher,
            fixture: "boot_server/dispatcher.php",
            env: &[("BOOT_PROBE", "from-env")],
            ini: "register_argc_argv = Off",
        },
        Case {
            name: "the entrypoint wins over an environment variable of the same name",
            mode: Mode::Dispatcher,
            fixture: "boot_server/dispatcher.php",
            env: &[("BOOT_PROBE", "from-env"), ("SCRIPT_FILENAME", "/from/env")],
            ini: "",
        },
    ];

    let shared = std::fs::read_to_string(fixture("ini/shared/php.ini"))?;
    for case in &cases {
        let mut spawn = Spawn::http(case.mode, fixture(case.fixture))
            .php_ini(&format!("{shared}\n{}\n", case.ini));
        for &(key, value) in case.env {
            spawn = spawn.env(key, value);
        }
        let srv = spawn.spawn();
        let (status, body) = drain(submit(srv.addr, req("/"))?);
        assert_eq!(status, 200, "{}: {body}", case.name);

        let name = case.fixture.rsplit('/').next().expect("file name");
        let entrypoint = srv.dir.join("http").join(name).display().to_string();
        let want = json!({
            "BOOT_PROBE": "from-env",
            "PHP_SELF": entrypoint,
            "SCRIPT_NAME": entrypoint,
            "SCRIPT_FILENAME": entrypoint,
            "PATH_TRANSLATED": entrypoint,
            "DOCUMENT_ROOT": "",
            "argv": [entrypoint],
            "argc": 1,
        });
        let got: Value = serde_json::from_str(&body)?;
        assert_eq!(got, want, "{}", case.name);
    }
    Ok(())
}
