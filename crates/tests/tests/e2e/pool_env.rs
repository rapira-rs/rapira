use std::time::Duration;

use rapira_config::Mode;
use serde_json::{Value, json};
use tests::wire::submit;
use tests::{StatusBody, drain, fixture, poll, req};

use crate::harness::{Server, Spawn, diagnostics, signal};

/// A server over `pool_env/worker.php` or `pool_env/classic.php`. Each `POOL_*` name of `env.php` has a fixed set of layers: the boot environment, the pool `env` table, both or none. The odd names test the shapes of the env arrays.
fn spawn(mode: Mode) -> Server {
    let entrypoint = match mode {
        Mode::Classic => "pool_env/classic.php",
        _ => "pool_env/worker.php",
    };
    Spawn::http(mode, fixture(entrypoint))
        .env("POOL_PROBE", "inherited")
        .env("POOL_INHERITED", "inherited")
        .env("POOL_DROP", "inherited")
        .env("POOL.DOT", "dot")
        .env("POOL SPACE", "space")
        .env("POOL[BR", "bracket")
        .env("4242", "num")
        .toml(
            "[http.pool.env]\nPOOL_PROBE = \"config\"\nPOOL_EMPTY = \"\"\nSERVER_SOFTWARE = \"config\"\n\
             POOL_ONLY = \"config\"\nPOOL_DROP = \"config\"\n\"POOL.CONF\" = \"conf\"\n\"4444\" = \"num\"",
        )
        .spawn()
}

/// The JSON body of a 200 answer to `GET path`.
fn get(srv: &Server, path: &str) -> anyhow::Result<Value> {
    let StatusBody { status, body } = drain(submit(srv.addr, req(path))?);
    assert_eq!(status, 200, "GET {path}: {body}\n{}", diagnostics(srv));
    Ok(serde_json::from_str(&body)?)
}

/// An answer of the `/view` route: getenv($name), getenv($name, true) and getenv() agree on each name.
fn view(names: Value) -> Value {
    json!({ "view": names, "mismatch": [] })
}

/// The pool `env` table wins over the boot environment.
fn before_put() -> Value {
    json!({
        "POOL_PROBE": "config",
        "POOL_INHERITED": "inherited",
        "POOL_EMPTY": "",
        "POOL_ONLY": "config",
        "POOL_DROP": "config",
        "POOL_NEW": false,
    })
}

/// putenv() wins over the two other layers. `N=` sets an empty value, `N` hides the name in both lower layers, and the value starts after the first `=`.
fn after_put() -> Value {
    json!({
        "POOL_PROBE": "put",
        "POOL_INHERITED": "inherited",
        "POOL_EMPTY": "",
        "POOL_ONLY": "",
        "POOL_DROP": false,
        "POOL_NEW": "a=b",
    })
}

/// An env pair of the pool wins over an inherited variable of the same name. The `$_SERVER` of each request gets the pairs, and a request variable with the same name wins. A change to a pair in one request does not reach the next request of the worker.
#[test]
fn pool_env_reaches_the_worker() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        mode: Mode,
        want: Value,
    }
    let cases = [
        Case {
            name: "worker mode: a pair wins over an inherited variable",
            mode: Mode::Worker,
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
        let srv = spawn(case.mode);
        for n in 1..=3 {
            assert_eq!(get(&srv, "/")?, case.want, "{} request {n}", case.name);
        }
    }
    Ok(())
}

/// getenv() reads the putenv() writes of the PHP request, then the pool `env` table, then the boot environment. In worker mode the app instance is one PHP request, so a write stays for the next units until a reload. In classic mode it ends with its request.
#[test]
fn putenv_lasts_until_the_php_request_ends() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        mode: Mode,
        next: Value,
    }
    let cases = [
        Case {
            name: "worker mode",
            mode: Mode::Worker,
            next: view(after_put()),
        },
        Case {
            name: "classic mode",
            mode: Mode::Classic,
            next: view(before_put()),
        },
    ];

    for case in &cases {
        let srv = spawn(case.mode);
        assert_eq!(
            get(&srv, "/view")?,
            view(before_put()),
            "{}: before putenv()",
            case.name
        );
        let mut put = view(after_put());
        put["put"] = json!(true);
        assert_eq!(
            get(&srv, "/put")?,
            put,
            "{}: the request of putenv()",
            case.name
        );
        assert_eq!(
            get(&srv, "/view")?,
            case.next,
            "{}: the next request",
            case.name
        );

        signal(srv.pid(), libc::SIGUSR2);
        let reset = poll(Duration::from_secs(20), || {
            get(&srv, "/view").ok().filter(|v| *v == view(before_put()))
        });
        assert!(
            reset.is_some(),
            "{}: after the reload\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
    Ok(())
}

/// A child process gets the environment of the server at boot. It does not get the pool `env` table or a putenv() write, and a putenv() removal does not reach it.
#[test]
fn a_child_process_gets_only_the_boot_env() -> anyhow::Result<()> {
    let srv = spawn(Mode::Worker);
    let boot = json!({
        "POOL_PROBE": "inherited",
        "POOL_INHERITED": "inherited",
        "POOL_EMPTY": false,
        "POOL_ONLY": false,
        "POOL_DROP": "inherited",
        "POOL_NEW": false,
    });
    assert_eq!(get(&srv, "/child")?, boot, "before putenv()");
    get(&srv, "/put")?;
    assert_eq!(get(&srv, "/child")?, boot, "after putenv()");
    Ok(())
}

/// `${NAME}` in INI text reads the same layers as getenv(). The getenv callback of a SAPI cannot report a removal, so a name that putenv() removed reads the boot environment.
/// https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_ini_parser.y#L172-L193
#[test]
fn ini_text_reads_the_env_layers() -> anyhow::Result<()> {
    let srv = spawn(Mode::Worker);
    assert_eq!(
        get(&srv, "/ini")?,
        json!({
            "POOL_PROBE": "config",
            "POOL_INHERITED": "inherited",
            "POOL_EMPTY": "",
            "POOL_ONLY": "config",
            "POOL_DROP": "config",
            "POOL_NEW": "undefined",
        }),
        "before putenv()"
    );
    get(&srv, "/put")?;
    assert_eq!(
        get(&srv, "/ini")?,
        json!({
            "POOL_PROBE": "put",
            "POOL_INHERITED": "inherited",
            "POOL_EMPTY": "",
            "POOL_ONLY": "",
            "POOL_DROP": "inherited",
            "POOL_NEW": "a=b",
        }),
        "after putenv()"
    );
    Ok(())
}

/// getenv($name) finds any name of every layer. getenv(), `$_ENV` and the boot `$_SERVER` skip a name with a space, a dot or a bracket, and key a numeric name as an integer, as php-src does. `$_ENV` and the boot `$_SERVER` do not see a later putenv(). The arguments behave as in php-src.
/// https://github.com/php/php-src/blob/php-8.5.11/main/php_variables.c#L592-L627
/// https://github.com/php/php-src/blob/php-8.5.11/ext/standard/basic_functions.c#L705-L763
#[test]
fn the_env_arrays_keep_the_php_src_shapes() -> anyhow::Result<()> {
    let srv = spawn(Mode::Worker);
    let syntax = "putenv(): Argument #1 ($assignment) must have a valid syntax";
    let boot = json!({ "odd": [], "int": [4242, 4444] });
    assert_eq!(
        get(&srv, "/shape")?,
        json!({
            "calls": [true, true, true, "named", true, true, false, syntax, syntax],
            "named": ["dot", "space", "bracket", "conf", "put", "num", "num", "num"],
            "getenv": { "odd": [], "int": [4242, 4343, 4444] },
            "env": boot,
            "server": boot,
        })
    );
    Ok(())
}
