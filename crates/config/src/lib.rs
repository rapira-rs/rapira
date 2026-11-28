use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod listen;
mod log;
mod pool;
mod server;

pub use listen::ListenAddr;
pub use log::{LogFormat, LogLevel, LogSettings, resolve_log};
pub use pool::{ExecutionClock, Mode, PoolSection, PoolSettings, check_entrypoint, resolve_pool};
pub use server::{ServerSection, ServerSettings, resolve_server};

/// `value` resolved against the config directory `dir`. An empty string counts as absent.
pub fn opt_path(dir: &Path, value: Option<&str>) -> std::io::Result<Option<PathBuf>> {
    value
        .filter(|v| !v.is_empty())
        .map(|v| std::path::absolute(dir.join(v)))
        .transpose()
}

/// `{table}.listen`, or `default` when the key is absent. A relative unix socket path resolves against the config directory `dir`.
pub fn parse_listen(
    dir: &Path,
    table: &str,
    raw: Option<&str>,
    default: ListenAddr,
) -> anyhow::Result<ListenAddr> {
    match raw {
        Some(s) => parse_required_listen(dir, table, s),
        None => Ok(resolve_listen(dir, default)?),
    }
}

/// Parses a required `{table}.listen` value and resolves a relative unix socket path against `dir`.
pub fn parse_required_listen(dir: &Path, table: &str, raw: &str) -> anyhow::Result<ListenAddr> {
    let addr = raw
        .parse::<ListenAddr>()
        .with_context(|| format!("invalid {table}.listen `{raw}`"))?;
    Ok(resolve_listen(dir, addr)?)
}

/// `addr` with a relative unix socket path resolved against the config directory `dir`.
fn resolve_listen(dir: &Path, addr: ListenAddr) -> std::io::Result<ListenAddr> {
    match addr {
        ListenAddr::Unix(path) => Ok(ListenAddr::Unix(std::path::absolute(dir.join(path))?)),
        tcp => Ok(tcp),
    }
}

/// A `_secs` key that must be at least 1 and at most `MAX_TIMEOUT_SECS`.
pub fn nonzero_timeout(table: &str, key: &str, secs: u64) -> anyhow::Result<Duration> {
    if secs == 0 {
        bail!("{table}.{key} must be at least 1");
    }
    capped_timeout(table, key, secs)
}

/// Caps each timeout before the server computes its deadline.
const MAX_TIMEOUT_SECS: u64 = 86_400;

pub fn capped_timeout(table: &str, key: &str, secs: u64) -> anyhow::Result<Duration> {
    if secs > MAX_TIMEOUT_SECS {
        bail!("{table}.{key} {secs} is too large (max {MAX_TIMEOUT_SECS})");
    }
    Ok(Duration::from_secs(secs))
}

/// One built-in pipeline stage (HTTP middleware or gRPC interceptor) validated against its config list.
pub struct Stage {
    pub name: &'static str,
    pub configured: bool,
    pub requires_table: bool,
}

/// Validates a stage list against its configured subtables: rejects duplicates, missing required tables, unknown names, and unreferenced tables.
pub fn check_stages(
    table: &str,
    key: &str,
    kind: &str,
    list: &[String],
    stages: &[Stage],
) -> anyhow::Result<()> {
    for (i, name) in list.iter().enumerate() {
        if list[..i].contains(name) {
            bail!("{table}.{key} lists \"{name}\" twice");
        }
    }

    for name in list {
        match stages.iter().find(|s| s.name == name.as_str()) {
            Some(stage) if stage.requires_table && !stage.configured => {
                bail!("{table}.{key} lists \"{name}\" but [{table}.{name}] is missing");
            }
            Some(_) => {}
            None => {
                let known: Vec<String> = stages.iter().map(|s| format!("\"{}\"", s.name)).collect();
                bail!(
                    "{table}.{key} entry \"{name}\" is unknown; known {kind}: {}",
                    known.join(", ")
                );
            }
        }
    }

    for stage in stages {
        if stage.configured && !list.iter().any(|n| n == stage.name) {
            bail!(
                "[{table}.{}] is configured but {table}.{key} does not list \"{}\"",
                stage.name,
                stage.name
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        raw: Option<&'static str>,
        want: ListenAddr,
    }

    #[test]
    fn parse_listen_resolves_a_unix_path_against_the_config_dir() {
        let default = ListenAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 8000)));
        let cases = [
            Case {
                name: "relative unix path",
                raw: Some("unix:run/http.sock"),
                want: ListenAddr::Unix(PathBuf::from("/w/run/http.sock")),
            },
            Case {
                name: "absolute unix path",
                raw: Some("unix:/run/http.sock"),
                want: ListenAddr::Unix(PathBuf::from("/run/http.sock")),
            },
            Case {
                name: "absent key",
                raw: None,
                want: default.clone(),
            },
        ];
        for case in cases {
            let got = parse_listen(Path::new("/w"), "http", case.raw, default.clone()).unwrap();
            assert_eq!(got, case.want, "{}", case.name);
        }
    }

    #[test]
    fn check_stages_enforces_list_and_table_rules() {
        struct StageCase {
            name: &'static str,
            list: &'static [&'static str],
            static_cfg: bool,
            compress_cfg: bool,
            error: Option<&'static str>,
        }
        let cases = [
            StageCase {
                name: "empty list with no tables",
                list: &[],
                static_cfg: false,
                compress_cfg: false,
                error: None,
            },
            StageCase {
                name: "optional table stage allowed without table",
                list: &["compress"],
                static_cfg: false,
                compress_cfg: false,
                error: None,
            },
            StageCase {
                name: "duplicate precedes unknown and missing table",
                list: &["unknown", "static", "static"],
                static_cfg: false,
                compress_cfg: false,
                error: Some("http.middleware lists \"static\" twice"),
            },
            StageCase {
                name: "missing required table",
                list: &["static"],
                static_cfg: false,
                compress_cfg: false,
                error: Some("http.middleware lists \"static\" but [http.static] is missing"),
            },
            StageCase {
                name: "unknown stage lists known names",
                list: &["bogus"],
                static_cfg: false,
                compress_cfg: false,
                error: Some(
                    "http.middleware entry \"bogus\" is unknown; known middleware: \"static\", \"compress\"",
                ),
            },
            StageCase {
                name: "configured table absent from list",
                list: &[],
                static_cfg: false,
                compress_cfg: true,
                error: Some(
                    "[http.compress] is configured but http.middleware does not list \"compress\"",
                ),
            },
        ];
        for case in cases {
            let list: Vec<String> = case.list.iter().map(|s| (*s).to_owned()).collect();
            let stages = [
                Stage {
                    name: "static",
                    configured: case.static_cfg,
                    requires_table: true,
                },
                Stage {
                    name: "compress",
                    configured: case.compress_cfg,
                    requires_table: false,
                },
            ];
            let res = check_stages("http", "middleware", "middleware", &list, &stages);
            match case.error {
                None => assert!(res.is_ok(), "{}: {res:?}", case.name),
                Some(want) => {
                    let err = res.expect_err(case.name).to_string();
                    assert_eq!(err, want, "{}", case.name);
                }
            }
        }
    }
}
