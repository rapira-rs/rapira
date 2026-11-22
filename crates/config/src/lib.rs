use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod listen;
mod log;
mod pool;
mod server;

pub use listen::ListenAddr;
pub use log::{LogFormat, LogLevel, LogSettings, resolve_log};
pub use pool::{Mode, PoolSection, PoolSettings, check_entrypoint, resolve_pool};
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
    let addr = match raw {
        Some(s) => s
            .parse::<ListenAddr>()
            .with_context(|| format!("invalid {table}.listen `{s}`"))?,
        None => default,
    };
    Ok(resolve_listen(dir, addr)?)
}

/// `addr` with a relative unix socket path resolved against the config directory `dir`.
pub fn resolve_listen(dir: &Path, addr: ListenAddr) -> std::io::Result<ListenAddr> {
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
}
