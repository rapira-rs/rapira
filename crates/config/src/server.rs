use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{nonzero_timeout, opt_path};

#[derive(Debug)]
pub struct ServerSettings {
    pub stop_timeout: Duration,
    pub pidfile: Option<PathBuf>,
}

impl ServerSettings {
    /// Leave time after the connection drain for PHP request and thread shutdown.
    pub fn drain_grace(&self) -> Duration {
        const MARGIN: Duration = Duration::from_secs(5);
        self.stop_timeout - MARGIN.min(self.stop_timeout / 2)
    }
}

/// The `[server]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pidfile: Option<String>,
    stop_timeout_secs: Option<u64>,
}

pub fn resolve_server(section: ServerSection, dir: &Path) -> anyhow::Result<ServerSettings> {
    let pidfile = opt_path(dir, section.pidfile.as_deref())?;

    Ok(ServerSettings {
        stop_timeout: nonzero_timeout(
            "server",
            "stop_timeout_secs",
            section.stop_timeout_secs.unwrap_or(30),
        )?,
        pidfile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(toml: &str) -> anyhow::Result<ServerSettings> {
        let section = toml::from_str(toml)?;
        resolve_server(section, Path::new("/w"))
    }

    #[test]
    fn server_pidfile_resolves_against_config_dir() {
        let s = server("pidfile = \"rapira.pid\"\n").unwrap();
        assert_eq!(s.pidfile, Some(PathBuf::from("/w/rapira.pid")));
    }

    struct Case {
        name: &'static str,
        toml: &'static str,
        error: &'static str,
    }

    /// The stop timeout must leave time for the drain.
    #[test]
    fn server_errors_name_the_key() {
        let cases = [
            Case {
                name: "zero stop timeout",
                toml: "stop_timeout_secs = 0\n",
                error: "server.stop_timeout_secs must be at least 1",
            },
            Case {
                name: "stop timeout above the cap",
                toml: "stop_timeout_secs = 100000\n",
                error: "server.stop_timeout_secs 100000 is too large (max 86400)",
            },
            Case {
                name: "unknown key",
                toml: "bogus = 1\n",
                error: "unknown field `bogus`",
            },
        ];
        for case in cases {
            let err = server(case.toml).expect_err(case.name).to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }

    #[test]
    fn drain_grace_leaves_time_for_shutdown() {
        struct Case {
            name: &'static str,
            secs: u64,
            millis: u64,
        }
        let cases = [
            Case {
                name: "five second margin",
                secs: 30,
                millis: 25_000,
            },
            Case {
                name: "half the budget",
                secs: 5,
                millis: 2500,
            },
            Case {
                name: "shortest budget",
                secs: 1,
                millis: 500,
            },
        ];
        for case in cases {
            let s = server(&format!("stop_timeout_secs={}", case.secs)).unwrap();
            assert_eq!(
                s.drain_grace(),
                Duration::from_millis(case.millis),
                "{}",
                case.name
            );
        }
    }
}
