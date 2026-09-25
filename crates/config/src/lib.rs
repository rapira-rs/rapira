use anyhow::{Context, bail};
use serde::Deserialize;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod grpc;
mod http;
mod listen;
mod log;
mod pool;
mod supervisor;

pub use grpc::GrpcSettings;
pub use http::{
    HttpSettings, MiddlewareSettings, StaticSettings, UnsafeFieldNames, UploadSettings,
};
pub use listen::Listen;
pub use log::{LogFormat, LogLevel, LogSettings};
pub use pool::{PoolSettings, RunMode, Scaling};
pub use supervisor::SupervisorSettings;

use grpc::{GrpcSection, resolve_grpc};
use http::{HttpSection, resolve_middleware, resolve_static, resolve_uploads};
use log::{LogSection, resolve_log};
use pool::resolve_pool;
use supervisor::{SupervisorSection, resolve_supervisor};

#[derive(Debug)]
pub struct Settings {
    pub http: Option<HttpSettings>,
    pub grpc: Option<GrpcSettings>,
    pub supervisor: SupervisorSettings,
    pub log: LogSettings,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    http: Option<HttpSection>,
    grpc: Option<GrpcSection>,
    #[serde(default)]
    supervisor: SupervisorSection,
    #[serde(default)]
    log: LogSection,
}

/// `{table}.listen`, or `default` when the key is absent.
fn parse_listen(table: &str, raw: Option<&str>, default: Listen) -> anyhow::Result<Listen> {
    match raw {
        Some(s) => s
            .parse::<Listen>()
            .with_context(|| format!("invalid {table}.listen `{s}`")),
        None => Ok(default),
    }
}

/// A `_secs` key that must be at least 1 and at most `MAX_TIMEOUT_SECS`.
fn nonzero_timeout(table: &str, key: &str, secs: u64) -> anyhow::Result<Duration> {
    if secs == 0 {
        bail!("{table}.{key} must be at least 1");
    }
    capped_timeout(table, key, secs)
}

pub fn resolve(path: &Path) -> anyhow::Result<Settings> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let file =
        load_str(&text).with_context(|| format!("parsing config file {}", path.display()))?;
    merge(file, path.parent())
}

fn load_str(text: &str) -> anyhow::Result<FileConfig> {
    Ok(toml::from_str(text)?)
}

fn merge(file: FileConfig, config_dir: Option<&Path>) -> anyhow::Result<Settings> {
    if file.http.is_none() && file.grpc.is_none() {
        bail!("no plugin configured: add an [http] or a [grpc] table");
    }
    let http = file
        .http
        .map(|section| resolve_http(section, config_dir))
        .transpose()?;
    let grpc = file
        .grpc
        .map(|section| resolve_grpc(section, config_dir))
        .transpose()?;
    let supervisor = resolve_supervisor(file.supervisor, config_dir)?;
    let log = resolve_log(file.log)?;

    Ok(Settings {
        http,
        grpc,
        supervisor,
        log,
    })
}

fn resolve_http(section: HttpSection, config_dir: Option<&Path>) -> anyhow::Result<HttpSettings> {
    let listen = parse_listen(
        "http",
        section.listen.as_deref(),
        Listen::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 8000))),
    )?;

    let server_port = match section.server_port {
        Some(p) => p,
        None => match &listen {
            Listen::Tcp(addr) => addr.port(),
            Listen::Unix(_) => 80,
        },
    };

    let max_body_size_mb = section.max_body_size_mb.unwrap_or(8);
    if max_body_size_mb == 0 {
        bail!("http.max_body_size_mb must be at least 1");
    }
    let max_body_size = max_body_size_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("http.max_body_size_mb {max_body_size_mb} is too large"))?;

    let write_timeout = nonzero_timeout(
        "http",
        "write_timeout_secs",
        section.write_timeout_secs.unwrap_or(30),
    )?;
    let keepalive_timeout = nonzero_timeout(
        "http",
        "keepalive_timeout_secs",
        section.keepalive_timeout_secs.unwrap_or(60),
    )?;

    let pool = resolve_pool(section.pool, "http.pool", config_dir)?;
    if section.uploads.is_some() && pool.mode != RunMode::Dispatcher {
        bail!(
            "http.uploads applies to dispatcher mode only (http.pool.mode = \"{}\")",
            pool.mode.as_str()
        );
    }
    let uploads = resolve_uploads(section.uploads.unwrap_or_default(), config_dir)?;

    let sendfile_root = match section.sendfile.root.filter(|r| !r.is_empty()) {
        Some(r) => config_relative(config_dir, &r)?,
        None => pool
            .entrypoint
            .parent()
            .unwrap_or(Path::new("/"))
            .to_path_buf(),
    };

    let static_files = section
        .r#static
        .map(|s| resolve_static(s, config_dir))
        .transpose()?;
    let middleware = resolve_middleware(section.middleware, static_files)?;

    Ok(HttpSettings {
        listen,
        server_name: section
            .server_name
            .unwrap_or_else(|| "localhost".to_owned()),
        server_port,
        max_body_size,
        write_timeout,
        keepalive_timeout,
        unsafe_field_names: section.unsafe_field_names.unwrap_or_default(),
        uploads,
        sendfile_root,
        middleware,
        pool,
    })
}

/// Caps every `*_secs` key so the master's deadline arithmetic can't overflow.
const MAX_TIMEOUT_SECS: u64 = 86_400;

fn capped_timeout(table: &str, key: &str, secs: u64) -> anyhow::Result<Duration> {
    if secs > MAX_TIMEOUT_SECS {
        bail!("{table}.{key} {secs} is too large (max {MAX_TIMEOUT_SECS})");
    }
    Ok(Duration::from_secs(secs))
}

fn config_relative(config_dir: Option<&Path>, value: &str) -> std::io::Result<PathBuf> {
    std::path::absolute(config_dir.unwrap_or_else(|| Path::new(".")).join(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `[http]` settings of `toml`, resolved against `dir`.
    fn http_of(toml: &str, dir: &str) -> HttpSettings {
        let file = load_str(toml).unwrap();
        merge(file, Some(Path::new(dir))).unwrap().http.unwrap()
    }

    #[test]
    fn http_pool_resolves_from_file() {
        let http = http_of(
            r#"
            [http]
            listen = "0.0.0.0:9000"
            [http.pool]
            processes = 2
            entrypoint = "app.php"
        "#,
            "/etc/rapira",
        );
        assert_eq!(http.listen.to_string(), "0.0.0.0:9000");
        assert_eq!(http.pool.processes, 2);
        assert_eq!(http.pool.entrypoint, Path::new("/etc/rapira/app.php"));
    }

    /// The pool belongs to its plugin. A top-level table names no plugin, so it must fail at parse time.
    #[test]
    fn top_level_pool_table_is_rejected() {
        let err = load_str("[pool]\nentrypoint = \"a.php\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `pool`"), "{err}");
        assert!(load_str("[http.pool]\nentrypoint = \"a.php\"\n").is_ok());
    }

    /// The e2e harness writes `[http.pool]` before `[http]`. TOML allows the super-table later.
    #[test]
    fn subtable_before_supertable_parses() {
        let file = load_str(
            "[http.pool]\nentrypoint = \"a.php\"\nprocesses = 3\n\
             [log]\nlevel = \"debug\"\n\
             [http]\nlisten = \"127.0.0.1:7000\"\nmiddleware = [\"static\"]\n\
             [http.static]\nroot = \"public\"\n",
        )
        .unwrap();
        let s = merge(file, Some(Path::new("/w"))).unwrap();
        let http = s.http.unwrap();
        assert_eq!(http.pool.processes, 3);
        assert_eq!(http.listen.to_string(), "127.0.0.1:7000");
        assert_eq!(s.log.level, LogLevel::Debug);
        let MiddlewareSettings::Static(st) = &http.middleware[0];
        assert_eq!(st.root, Path::new("/w/public"));
    }

    #[test]
    fn server_port_derives_from_listen_and_mb_converts() {
        let http = http_of(
            "[http]\nlisten = \":9000\"\nmax_body_size_mb = 2\n[http.pool]\nentrypoint = \"a.php\"\n",
            "/w",
        );
        assert_eq!(http.server_port, 9000);
        assert_eq!(http.max_body_size, 2 * 1024 * 1024);

        let http = http_of(
            "[http]\nlisten = \"unix:/run/r.sock\"\n[http.pool]\nentrypoint = \"a.php\"\n",
            "/w",
        );
        assert_eq!(http.server_port, 80);
    }

    #[test]
    fn unsafe_field_names_parses_and_defaults_to_drop() {
        for (text, want) in [
            ("drop", UnsafeFieldNames::Drop),
            ("reject", UnsafeFieldNames::Reject),
        ] {
            let http = http_of(
                &format!(
                    "[http]\nunsafe_field_names = \"{text}\"\n[http.pool]\nentrypoint = \"a.php\"\n"
                ),
                "/w",
            );
            assert_eq!(http.unsafe_field_names, want, "{text}");
        }

        let http = http_of("[http.pool]\nentrypoint = \"a.php\"\n", "/w");
        assert_eq!(http.unsafe_field_names, UnsafeFieldNames::Drop);
    }

    /// `allow` is rejected too: there is no off-switch, so asking for one must fail loudly.
    #[test]
    fn unknown_unsafe_field_names_value_is_rejected() {
        for value in ["dorp", "allow"] {
            assert!(
                load_str(&format!(
                    "[http]\nunsafe_field_names = \"{value}\"\n[http.pool]\nentrypoint = \"a.php\"\n"
                ))
                .is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn file_entrypoint_is_config_dir_relative() {
        let http = http_of(
            "[http.pool]\nentrypoint = \"public/index.php\"\n",
            "/srv/app",
        );
        assert_eq!(
            http.pool.entrypoint,
            std::path::absolute("/srv/app/public/index.php").unwrap()
        );
    }

    #[test]
    fn entrypoint_is_required() {
        let file = load_str("[http.pool]\nentrypoint = \"\"\n").unwrap();
        let err = merge(file, Some(Path::new("/srv/app")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("http.pool.entrypoint is required"), "{err}");

        let file = load_str("[http.pool]\n").unwrap();
        let err = merge(file, Some(Path::new("/srv/app")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("http.pool.entrypoint is required"), "{err}");
    }

    /// The root resolves config-relative; without the key it follows the entrypoint.
    #[test]
    fn http_sendfile_root_defaults_to_the_entrypoint_dir() {
        let http = http_of(
            "[http.pool]\nentrypoint = \"public/index.php\"\n",
            "/srv/app",
        );
        assert_eq!(http.sendfile_root, Path::new("/srv/app/public"));

        let http = http_of(
            "[http.pool]\nentrypoint = \"public/index.php\"\n[http.sendfile]\nroot = \"assets\"\n",
            "/srv/app",
        );
        assert_eq!(http.sendfile_root, Path::new("/srv/app/assets"));
    }

    /// The shipped example is the file users copy, so it must resolve and keep its documented values.
    #[test]
    fn shipped_example_config_resolves() {
        let http = http_of(include_str!("../../../examples/rapira.toml"), "/srv/app");
        assert_eq!(http.listen.to_string(), "127.0.0.1:8000");
        assert_eq!(http.server_port, 8000);
        assert_eq!(
            http.pool.entrypoint,
            Path::new("/srv/app/dispatcher-sync.php")
        );
        assert_eq!(http.pool.mode, RunMode::Dispatcher);
        assert_eq!(http.sendfile_root, Path::new("/srv/app"));
    }

    #[test]
    fn max_body_size_overflow_is_rejected() {
        let file = load_str(
            "[http]\nmax_body_size_mb = 17592186044416\n[http.pool]\nentrypoint = \"a.php\"\n",
        )
        .unwrap();
        let err = merge(file, Some(Path::new("/w"))).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(load_str("[http.pool]\nbogus = 1\n").is_err());
        assert!(load_str("[nope]\nx = 1\n").is_err());
        assert!(load_str("[supervisor]\nbogus = 1\n").is_err());
        assert!(load_str("[http.pool]\nthreads = 1\n").is_err());
        assert!(load_str("[http.pool]\nclassic = true\n").is_err());
        assert!(load_str("[pm]\nmode = \"static\"\n").is_err());
        assert!(load_str("[http.pool]\npidfile = \"r.pid\"\n").is_err());
        assert!(load_str("[supervisor]\nmax_requests = 1\n").is_err());
        assert!(load_str("[log]\nbogus = 1\n").is_err());
        assert!(load_str("[log]\nlevel = \"verbose\"\n").is_err());
        assert!(load_str("[log]\nformat = \"pretty\"\n").is_err());
        assert!(load_str("[http.static]\nbogus = 1\n").is_err());
    }

    #[test]
    fn timeout_caps_name_the_key_that_broke() {
        for (toml, key) in [
            (
                "[http.pool]\nentrypoint = \"a.php\"\nprocess_idle_timeout_secs = 100000\n",
                "http.pool.process_idle_timeout_secs",
            ),
            (
                "[http.pool]\nentrypoint = \"a.php\"\nrequest_terminate_timeout_secs = 100000\n",
                "http.pool.request_terminate_timeout_secs",
            ),
            (
                "[http.pool]\nentrypoint = \"a.php\"\n[supervisor]\nprocess_control_timeout_secs = 100000\n",
                "supervisor.process_control_timeout_secs",
            ),
            (
                "[http]\nwrite_timeout_secs = 100000\n[http.pool]\nentrypoint = \"a.php\"\n",
                "http.write_timeout_secs",
            ),
            (
                "[http]\nkeepalive_timeout_secs = 100000\n[http.pool]\nentrypoint = \"a.php\"\n",
                "http.keepalive_timeout_secs",
            ),
            (
                "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\nmax_timeout_secs = 100000\n[grpc.pool]\nentrypoint = \"g.php\"\n",
                "grpc.max_timeout_secs",
            ),
        ] {
            let err = merge(load_str(toml).unwrap(), Some(Path::new("/w")))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(key) && err.contains("too large"),
                "{key}: {err}"
            );
        }

        let file =
            load_str("[http.pool]\nentrypoint = \"a.php\"\nprocess_idle_timeout_secs = 86400\n")
                .unwrap();
        assert!(merge(file, Some(Path::new("/w"))).is_ok());
    }

    /// A zero stop budget escalates at once and leaves the drain no time.
    #[test]
    fn supervisor_control_timeout_zero_is_rejected() {
        let file = load_str(
            "[http.pool]\nentrypoint = \"a.php\"\n[supervisor]\nprocess_control_timeout_secs = 0\n",
        )
        .unwrap();
        let err = merge(file, Some(Path::new("/w"))).unwrap_err().to_string();
        assert!(
            err.contains("supervisor.process_control_timeout_secs must be at least 1"),
            "{err}"
        );
    }

    #[test]
    fn http_pool_processes_zero_is_rejected() {
        let file = load_str("[http.pool]\nprocesses = 0\nentrypoint = \"a.php\"\n").unwrap();
        let err = merge(file, Some(Path::new("/w"))).unwrap_err().to_string();
        assert!(
            err.contains("http.pool.processes must be at least 1"),
            "{err}"
        );
    }

    /// Every knob resolves with unit conversion and a config-relative dir; a zero max_files would 413 every file part while booting clean.
    #[test]
    fn http_uploads_resolve_and_reject_zero_files() {
        let http = http_of(
            "[http.pool]\nentrypoint = \"a.php\"\n[http.uploads]\ndir = \"spool\"\n\
             max_file_size_mb = 3\nmax_field_size_kb = 7\nmax_files = 4\n\
             max_parts = 9\nmax_part_headers = 5\n",
            "/w",
        );
        let u = &http.uploads;
        assert_eq!(u.dir, Path::new("/w/spool"));
        assert_eq!(u.max_file_size, 3 * 1024 * 1024);
        assert_eq!(u.max_field_size, 7 * 1024);
        assert_eq!(u.max_files, 4);
        assert_eq!(u.max_parts, 9);
        assert_eq!(u.max_part_headers, 5);

        let file = load_str("[http.pool]\nentrypoint = \"a.php\"\n[http.uploads]\nmax_files = 0\n")
            .unwrap();
        let err = merge(file, Some(Path::new("/w"))).unwrap_err().to_string();
        assert!(
            err.contains("http.uploads.max_files must be at least 1"),
            "{err}"
        );
    }

    /// `ondemand` and `static` share one match arm that must still tell them apart and reject the dynamic-only spare keys.
    #[test]
    fn pool_ondemand_and_static_scaling_resolve() {
        for (key, want) in [("ondemand", Scaling::Ondemand), ("static", Scaling::Static)] {
            let http = http_of(
                &format!("[http.pool]\nentrypoint = \"a.php\"\nscaling = \"{key}\"\n"),
                "/w",
            );
            assert_eq!(http.pool.scaling, want, "{key}");
        }

        let file = load_str(
            "[http.pool]\nentrypoint = \"a.php\"\nscaling = \"ondemand\"\nmax_spare = 2\n",
        )
        .unwrap();
        let err = merge(file, Some(Path::new("/w"))).unwrap_err().to_string();
        assert!(err.contains("only valid with http.pool.scaling"), "{err}");
    }

    #[test]
    fn pool_run_mode_resolves() {
        for (key, want) in [
            ("classic", RunMode::Classic),
            ("worker", RunMode::Worker),
            ("dispatcher", RunMode::Dispatcher),
        ] {
            let http = http_of(
                &format!("[http.pool]\nentrypoint = \"a.php\"\nmode = \"{key}\"\n"),
                "/w",
            );
            assert_eq!(http.pool.mode, want, "{key}");
        }

        let http = http_of("[http.pool]\nentrypoint = \"a.php\"\n", "/w");
        assert_eq!(http.pool.mode, RunMode::Dispatcher);

        assert!(load_str("[http.pool]\nentrypoint = \"a.php\"\nmode = \"async\"\n").is_err());
    }

    /// An explicit table under any mode but dispatcher is a boot error; absence stays silent.
    #[test]
    fn http_uploads_require_dispatcher_mode() {
        for mode in ["classic", "worker"] {
            let file = load_str(&format!(
                "[http.pool]\nentrypoint = \"a.php\"\nmode = \"{mode}\"\n[http.uploads]\nmax_files = 4\n"
            ))
            .unwrap();
            let err = merge(file, Some(Path::new("/w"))).unwrap_err().to_string();
            assert!(
                err.contains("dispatcher mode only")
                    && err.contains(&format!("http.pool.mode = \"{mode}\"")),
                "{err}"
            );
        }

        let file = load_str("[http.pool]\nentrypoint = \"a.php\"\n[http.uploads]\n").unwrap();
        assert!(merge(file, Some(Path::new("/w"))).is_ok());

        let file = load_str("[http.pool]\nentrypoint = \"a.php\"\nmode = \"classic\"\n").unwrap();
        assert!(merge(file, Some(Path::new("/w"))).is_ok());
    }

    #[test]
    fn pool_dynamic_requires_valid_spares() {
        let merged = |keys: &str| {
            let file = load_str(&format!(
                "[http.pool]\nprocesses = 4\nentrypoint = \"a.php\"\n{keys}"
            ))
            .unwrap();
            merge(file, Some(Path::new("/w")))
        };

        let err = merged("scaling = \"dynamic\"\n").unwrap_err().to_string();
        assert!(
            err.contains("http.pool.min_spare and http.pool.max_spare"),
            "{err}"
        );
        assert!(merged("scaling = \"dynamic\"\nmin_spare = 3\nmax_spare = 2\n").is_err());

        let err = merged("scaling = \"dynamic\"\nmin_spare = 1\nmax_spare = 5\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("http.pool spares") && err.contains("http.pool.processes (4)"),
            "{err}"
        );

        let err = merged("scaling = \"static\"\nmin_spare = 1\nmax_spare = 2\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only valid with http.pool.scaling"), "{err}");

        let http =
            merged("scaling = \"dynamic\"\nmin_spare = 1\nmax_spare = 3\nmax_requests = 500\n")
                .unwrap()
                .http
                .unwrap();
        assert_eq!(
            http.pool.scaling,
            Scaling::Dynamic {
                min_spare: 1,
                max_spare: 3
            }
        );
        assert_eq!(http.pool.max_requests, 500);
    }

    #[test]
    fn supervisor_pidfile_resolves_against_config_dir() {
        let file = load_str(
            "[http.pool]\nentrypoint = \"a.php\"\n[supervisor]\npidfile = \"rapira.pid\"\n",
        )
        .unwrap();
        let s = merge(file, Some(Path::new("/etc/rapira"))).unwrap();
        assert_eq!(
            s.supervisor.pidfile.as_deref(),
            Some(Path::new("/etc/rapira/rapira.pid"))
        );
    }

    /// The root resolves config-relative like every other path key; forbid defaults to the PHP source guard.
    #[test]
    fn http_static_resolves_with_defaults() {
        let http = http_of(
            "[http.pool]\nentrypoint = \"a.php\"\n[http]\nmiddleware = [\"static\"]\n[http.static]\nroot = \"public\"\n",
            "/w",
        );
        let MiddlewareSettings::Static(st) = &http.middleware[0];
        assert_eq!(st.root, Path::new("/w/public"));
        assert_eq!(st.forbid, vec![".php".to_owned()]);

        let http = http_of("[http.pool]\nentrypoint = \"a.php\"\n", "/w");
        assert!(http.middleware.is_empty());

        let http = http_of(
            "[http.pool]\nentrypoint = \"a.php\"\n[http]\nmiddleware = [\"static\"]\n[http.static]\nroot = \"/srv/pub\"\n",
            "/w",
        );
        let MiddlewareSettings::Static(st) = &http.middleware[0];
        assert_eq!(st.root, Path::new("/srv/pub"));
    }

    /// The list is the activation switch: every configured section must be listed, every listed name must be known, configured, and unique.
    #[test]
    fn http_middleware_list_validates() {
        for (toml, needle) in [
            (
                "[http]\nmiddleware = [\"staticc\"]\n",
                "\"staticc\" is unknown",
            ),
            (
                "[http]\nmiddleware = [\"static\"]\n",
                "[http.static] is missing",
            ),
            (
                "[http]\nmiddleware = [\"static\", \"static\"]\n[http.static]\nroot = \"p\"\n",
                "twice",
            ),
            ("[http.static]\nroot = \"p\"\n", "does not list \"static\""),
        ] {
            let file = load_str(&format!("[http.pool]\nentrypoint = \"a.php\"\n{toml}")).unwrap();
            let err = merge(file, Some(Path::new("/w"))).unwrap_err().to_string();
            assert!(err.contains(needle), "{toml}: {err}");
        }
    }

    #[test]
    fn http_static_requires_root() {
        for toml in [
            "[http.pool]\nentrypoint = \"a.php\"\n[http.static]\n",
            "[http.pool]\nentrypoint = \"a.php\"\n[http.static]\nroot = \"\"\n",
        ] {
            let err = merge(load_str(toml).unwrap(), Some(Path::new("/w")))
                .unwrap_err()
                .to_string();
            assert!(err.contains("http.static.root"), "{err}");
        }
    }

    /// The resolver validates shape only. The middleware constructor normalizes the case.
    #[test]
    fn http_static_forbid_validates() {
        let http = http_of(
            "[http.pool]\nentrypoint = \"a.php\"\n[http]\nmiddleware = [\"static\"]\n[http.static]\nroot = \"p\"\nforbid = [\".PHP\", \".Phtml\"]\n",
            "/w",
        );
        let MiddlewareSettings::Static(st) = &http.middleware[0];
        assert_eq!(st.forbid, vec![".PHP".to_owned(), ".Phtml".to_owned()]);

        let http = http_of(
            "[http.pool]\nentrypoint = \"a.php\"\n[http]\nmiddleware = [\"static\"]\n[http.static]\nroot = \"p\"\nforbid = []\n",
            "/w",
        );
        let MiddlewareSettings::Static(st) = &http.middleware[0];
        assert!(st.forbid.is_empty());

        for entry in ["php", "", ".", ".php ", "./php"] {
            let file = load_str(&format!(
                "[http.pool]\nentrypoint = \"a.php\"\n[http.static]\nroot = \"p\"\nforbid = [\"{entry}\"]\n"
            ))
            .unwrap();
            let err = merge(file, Some(Path::new("/w"))).unwrap_err().to_string();
            assert!(err.contains("http.static.forbid"), "{entry}: {err}");
        }
    }

    /// The filter string is assembled from these keys, so a key carrying filter syntax would inject directives (`"php=trace,tokio" = "debug"` reads as two).
    #[test]
    fn log_target_names_that_would_corrupt_the_filter_are_rejected() {
        for entry in [
            "\"\" = \"info\"",
            "\"php=trace,tokio\" = \"info\"",
            "\"a b\" = \"info\"",
            "\"a/b\" = \"info\"",
            "\"a\\u001Bb\" = \"info\"",
            "\"http[request]\" = \"info\"",
            "\".php\" = \"info\"",
        ] {
            let file = load_str(&format!(
                "[http.pool]\nentrypoint = \"a.php\"\n[log.targets]\n{entry}\n"
            ))
            .unwrap();
            assert!(merge(file, Some(Path::new("/w"))).is_err(), "{entry}");
        }
    }
}
