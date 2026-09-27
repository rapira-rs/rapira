use anyhow::bail;
use serde::Deserialize;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{ConfigCtx, capped_timeout};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolSettings {
    pub entrypoint: PathBuf,
    pub processes: usize,
    pub mode: Mode,
    /// Requests a worker serves before recycling itself (with jitter); 0 = unlimited.
    pub max_requests: u64,
    /// Wall-clock bound on a single request; zero = disabled.
    pub request_terminate_timeout: Duration,
}

/// The pool mode of a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Classic,
    Worker,
    Dispatcher,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Classic => "classic",
            Self::Worker => "worker",
            Self::Dispatcher => "dispatcher",
        })
    }
}

/// The `pool` table of a plugin. Embedded by name: serde does not support `#[serde(flatten)]` alongside `deny_unknown_fields`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolSection {
    entrypoint: Option<String>,
    processes: Option<usize>,
    mode: Option<Mode>,
    max_requests: Option<u64>,
    request_terminate_timeout_secs: Option<u64>,
}

fn default_processes() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// `table` is the qualified table of the calling plugin, such as `http.pool`. Every message carries it.
pub fn resolve_pool(
    section: PoolSection,
    table: &str,
    ctx: &ConfigCtx,
) -> anyhow::Result<PoolSettings> {
    let processes = section.processes.unwrap_or_else(default_processes);
    if processes == 0 {
        bail!("{table}.processes must be at least 1");
    }

    let mode = section.mode.unwrap_or(Mode::Dispatcher);

    let Some(ep) = section.entrypoint.as_deref().filter(|s| !s.is_empty()) else {
        bail!("{table}.entrypoint is required");
    };
    let entrypoint = ctx.resolve_path(ep)?;

    Ok(PoolSettings {
        entrypoint,
        processes,
        mode,
        max_requests: section.max_requests.unwrap_or(0),
        request_terminate_timeout: capped_timeout(
            table,
            "request_terminate_timeout_secs",
            section.request_terminate_timeout_secs.unwrap_or(0),
        )?,
    })
}

/// `table` is the pool table that names the entrypoint, for the error text.
pub fn check_entrypoint(table: &str, entrypoint: &Path) -> anyhow::Result<()> {
    // The entrypoint is fixed for the pool's lifetime, so one open at boot covers every request.
    // The open proves read permission; the metadata check rejects a directory.
    let meta = File::open(entrypoint)
        .and_then(|f| f.metadata())
        .map_err(|e| {
            anyhow::anyhow!(
                "{table}.entrypoint {} is not readable: {e}",
                entrypoint.display()
            )
        })?;
    anyhow::ensure!(
        meta.is_file(),
        "{table}.entrypoint {} is not a regular file",
        entrypoint.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ConfigCtx {
        ConfigCtx {
            dir: PathBuf::from("/w"),
        }
    }

    /// The pool of `toml` under the table name the http plugin passes.
    fn pool(toml: &str) -> anyhow::Result<PoolSettings> {
        let section: PoolSection = toml::from_str(toml)?;
        resolve_pool(section, "http.pool", &ctx())
    }

    /// What `entrypoint = "a.php"` and `processes = 4` resolve to.
    fn base() -> PoolSettings {
        PoolSettings {
            entrypoint: PathBuf::from("/w/a.php"),
            processes: 4,
            mode: Mode::Dispatcher,
            max_requests: 0,
            request_terminate_timeout: Duration::ZERO,
        }
    }

    /// Every message carries the caller's table, so a second pool reports its own keys.
    #[test]
    fn resolve_pool_prefixes_errors_with_the_table() {
        let err = resolve_pool(PoolSection::default(), "grpc.pool", &ctx())
            .unwrap_err()
            .to_string();
        assert_eq!(err, "grpc.pool.entrypoint is required");
    }

    struct Case {
        name: &'static str,
        toml: &'static str,
        want: PoolSettings,
    }

    #[test]
    fn pool_keys_resolve() {
        let cases = [
            Case {
                name: "defaults",
                toml: "entrypoint = \"a.php\"\nprocesses = 4\n",
                want: base(),
            },
            Case {
                name: "entrypoint resolves against the config dir",
                toml: "entrypoint = \"public/index.php\"\nprocesses = 4\n",
                want: PoolSettings {
                    entrypoint: PathBuf::from("/w/public/index.php"),
                    ..base()
                },
            },
            Case {
                name: "max requests",
                toml: "entrypoint = \"a.php\"\nprocesses = 4\nmax_requests = 500\n",
                want: PoolSettings {
                    max_requests: 500,
                    ..base()
                },
            },
            Case {
                name: "classic mode",
                toml: "entrypoint = \"a.php\"\nprocesses = 4\nmode = \"classic\"\n",
                want: PoolSettings {
                    mode: Mode::Classic,
                    ..base()
                },
            },
            Case {
                name: "worker mode",
                toml: "entrypoint = \"a.php\"\nprocesses = 4\nmode = \"worker\"\n",
                want: PoolSettings {
                    mode: Mode::Worker,
                    ..base()
                },
            },
            Case {
                name: "dispatcher mode",
                toml: "entrypoint = \"a.php\"\nprocesses = 4\nmode = \"dispatcher\"\n",
                want: base(),
            },
            Case {
                name: "request timeout at the cap",
                toml: "entrypoint = \"a.php\"\nprocesses = 4\nrequest_terminate_timeout_secs = 86400\n",
                want: PoolSettings {
                    request_terminate_timeout: Duration::from_secs(86_400),
                    ..base()
                },
            },
        ];
        for case in cases {
            let got = pool(case.toml).unwrap_or_else(|e| panic!("{}: {e}", case.name));
            assert_eq!(got, case.want, "{}", case.name);
        }
    }

    struct ErrCase {
        name: &'static str,
        toml: &'static str,
        error: &'static str,
    }

    #[test]
    fn pool_errors_name_the_key() {
        let cases = [
            ErrCase {
                name: "empty entrypoint",
                toml: "entrypoint = \"\"\n",
                error: "http.pool.entrypoint is required",
            },
            ErrCase {
                name: "no entrypoint",
                toml: "",
                error: "http.pool.entrypoint is required",
            },
            ErrCase {
                name: "zero processes",
                toml: "entrypoint = \"a.php\"\nprocesses = 0\n",
                error: "http.pool.processes must be at least 1",
            },
            ErrCase {
                name: "request timeout above the cap",
                toml: "entrypoint = \"a.php\"\nrequest_terminate_timeout_secs = 100000\n",
                error: "http.pool.request_terminate_timeout_secs 100000 is too large (max 86400)",
            },
            ErrCase {
                name: "unknown mode",
                toml: "entrypoint = \"a.php\"\nmode = \"async\"\n",
                error: "unknown variant `async`",
            },
            ErrCase {
                name: "unknown key",
                toml: "bogus = 1\n",
                error: "unknown field `bogus`",
            },
            ErrCase {
                name: "threads is not a pool key",
                toml: "threads = 1\n",
                error: "unknown field `threads`",
            },
            ErrCase {
                name: "classic is not a pool key",
                toml: "classic = true\n",
                error: "unknown field `classic`",
            },
            ErrCase {
                name: "pidfile belongs to the supervisor",
                toml: "pidfile = \"r.pid\"\n",
                error: "unknown field `pidfile`",
            },
            ErrCase {
                name: "scaling is not a pool key",
                toml: "entrypoint = \"a.php\"\nscaling = \"static\"\n",
                error: "unknown field `scaling`",
            },
            ErrCase {
                name: "process_idle_timeout_secs is not a pool key",
                toml: "entrypoint = \"a.php\"\nprocess_idle_timeout_secs = 10\n",
                error: "unknown field `process_idle_timeout_secs`",
            },
        ];
        for case in cases {
            let err = pool(case.toml).expect_err(case.name).to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }
}
