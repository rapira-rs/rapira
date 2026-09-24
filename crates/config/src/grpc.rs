use anyhow::{Context, bail};
use serde::Deserialize;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::listen::Listen;
use crate::pool::{PoolSection, PoolSettings, RunMode, resolve_pool};
use crate::{capped_timeout, config_relative};

#[derive(Debug)]
pub struct GrpcSettings {
    pub listen: Listen,
    pub descriptor_set: PathBuf,
    pub services: Vec<String>,
    pub reflection: bool,
    /// Deadline for a call that sends no timeout.
    pub default_timeout: Option<Duration>,
    /// Upper bound on the deadline a client asks for.
    pub max_timeout: Option<Duration>,
    pub pool: PoolSettings,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrpcSection {
    listen: Option<String>,
    descriptor_set: Option<String>,
    services: Option<Vec<String>>,
    reflection: Option<bool>,
    default_timeout_secs: Option<u64>,
    max_timeout_secs: Option<u64>,
    #[serde(default)]
    pool: PoolSection,
}

pub(crate) fn resolve_grpc(
    section: GrpcSection,
    config_dir: Option<&Path>,
) -> anyhow::Result<GrpcSettings> {
    let listen = match section.listen.as_deref() {
        Some(s) => s
            .parse::<Listen>()
            .with_context(|| format!("invalid grpc.listen `{s}`"))?,
        None => Listen::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 50051))),
    };

    let Some(ds) = section.descriptor_set.as_deref().filter(|s| !s.is_empty()) else {
        bail!("grpc.descriptor_set is required");
    };
    let descriptor_set = config_relative(config_dir, ds)?;

    let services = section.services.unwrap_or_default();
    if services.is_empty() {
        bail!("grpc.services must name at least one service");
    }

    let default_timeout = optional_timeout("default_timeout_secs", section.default_timeout_secs)?;
    let max_timeout = optional_timeout("max_timeout_secs", section.max_timeout_secs)?;
    // The deadline policy returns the default without a clamp to the max.
    if let (Some(default), Some(max)) = (section.default_timeout_secs, section.max_timeout_secs)
        && default > max
    {
        bail!("grpc.default_timeout_secs ({default}) exceeds grpc.max_timeout_secs ({max})");
    }

    let pool = resolve_pool(section.pool, "grpc.pool", config_dir)?;
    if pool.mode != RunMode::Dispatcher {
        bail!(
            "grpc.pool.mode must be \"dispatcher\" (got \"{}\")",
            pool.mode.as_str()
        );
    }

    Ok(GrpcSettings {
        listen,
        descriptor_set,
        services,
        reflection: section.reflection.unwrap_or(false),
        default_timeout,
        max_timeout,
        pool,
    })
}

fn optional_timeout(key: &str, secs: Option<u64>) -> anyhow::Result<Option<Duration>> {
    let Some(secs) = secs else {
        return Ok(None);
    };
    if secs == 0 {
        bail!("grpc.{key} must be at least 1");
    }
    capped_timeout("grpc", key, secs).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{load_str, merge};

    struct Want {
        http: bool,
        listen: Listen,
        descriptor_set: &'static str,
        services: &'static [&'static str],
        reflection: bool,
        default_timeout: Option<Duration>,
        max_timeout: Option<Duration>,
    }

    struct Case {
        name: &'static str,
        toml: &'static str,
        expected: Result<Want, &'static str>,
    }

    /// Every resolved grpc pool runs in dispatcher mode, so a success case does not list the mode.
    #[test]
    fn grpc_table_resolves_and_validates() {
        let cases = [
            Case {
                name: "grpc only",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n\
                       [grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Ok(Want {
                    http: false,
                    listen: Listen::Tcp(SocketAddr::from(([127, 0, 0, 1], 50051))),
                    descriptor_set: "/w/a.binpb",
                    services: &["p.S"],
                    reflection: false,
                    default_timeout: None,
                    max_timeout: None,
                }),
            },
            Case {
                name: "both tables",
                toml: "[http.pool]\nentrypoint = \"a.php\"\n\
                       [grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n\
                       [grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Ok(Want {
                    http: true,
                    listen: Listen::Tcp(SocketAddr::from(([127, 0, 0, 1], 50051))),
                    descriptor_set: "/w/a.binpb",
                    services: &["p.S"],
                    reflection: false,
                    default_timeout: None,
                    max_timeout: None,
                }),
            },
            Case {
                name: "no plugin",
                toml: "[log]\nlevel = \"info\"\n",
                expected: Err("no plugin configured: add an [http] or a [grpc] table"),
            },
            Case {
                name: "descriptor_set missing",
                toml: "[grpc]\nservices = [\"p.S\"]\n[grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Err("grpc.descriptor_set is required"),
            },
            Case {
                name: "services empty",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = []\n\
                       [grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Err("grpc.services must name at least one service"),
            },
            Case {
                name: "worker mode refused",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n\
                       [grpc.pool]\nentrypoint = \"g.php\"\nmode = \"worker\"\n",
                expected: Err("grpc.pool.mode must be \"dispatcher\" (got \"worker\")"),
            },
            Case {
                name: "bad listen",
                toml: "[grpc]\nlisten = \"x\"\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n\
                       [grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Err("invalid grpc.listen `x`"),
            },
            Case {
                name: "zero default timeout",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n\
                       default_timeout_secs = 0\n[grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Err("grpc.default_timeout_secs must be at least 1"),
            },
            Case {
                name: "max over the cap",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n\
                       max_timeout_secs = 86401\n[grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Err("grpc.max_timeout_secs 86401 is too large (max 86400)"),
            },
            Case {
                name: "default above max",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n\
                       default_timeout_secs = 60\nmax_timeout_secs = 10\n\
                       [grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Err("grpc.default_timeout_secs (60) exceeds grpc.max_timeout_secs (10)"),
            },
            Case {
                name: "timeouts resolve",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n\
                       default_timeout_secs = 5\nmax_timeout_secs = 30\n\
                       [grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Ok(Want {
                    http: false,
                    listen: Listen::Tcp(SocketAddr::from(([127, 0, 0, 1], 50051))),
                    descriptor_set: "/w/a.binpb",
                    services: &["p.S"],
                    reflection: false,
                    default_timeout: Some(Duration::from_secs(5)),
                    max_timeout: Some(Duration::from_secs(30)),
                }),
            },
            Case {
                name: "unix listen",
                toml: "[grpc]\nlisten = \"unix:/run/g.sock\"\ndescriptor_set = \"a.binpb\"\n\
                       services = [\"p.S\"]\n[grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Ok(Want {
                    http: false,
                    listen: Listen::Unix(PathBuf::from("/run/g.sock")),
                    descriptor_set: "/w/a.binpb",
                    services: &["p.S"],
                    reflection: false,
                    default_timeout: None,
                    max_timeout: None,
                }),
            },
            Case {
                name: "unknown key",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\nfoo = 1\n\
                       [grpc.pool]\nentrypoint = \"g.php\"\n",
                expected: Err("unknown field `foo`"),
            },
            Case {
                name: "missing entrypoint",
                toml: "[grpc]\ndescriptor_set = \"a.binpb\"\nservices = [\"p.S\"]\n",
                expected: Err("grpc.pool.entrypoint is required"),
            },
        ];
        for case in cases {
            let got = load_str(case.toml).and_then(|file| merge(file, Some(Path::new("/w"))));
            match (got, case.expected) {
                (Ok(s), Ok(want)) => {
                    assert_eq!(s.http.is_some(), want.http, "{}", case.name);
                    let g = s.grpc.expect(case.name);
                    assert_eq!(g.listen, want.listen, "{}", case.name);
                    assert_eq!(
                        g.descriptor_set,
                        Path::new(want.descriptor_set),
                        "{}",
                        case.name
                    );
                    assert_eq!(g.services, want.services, "{}", case.name);
                    assert_eq!(g.reflection, want.reflection, "{}", case.name);
                    assert_eq!(g.default_timeout, want.default_timeout, "{}", case.name);
                    assert_eq!(g.max_timeout, want.max_timeout, "{}", case.name);
                    assert_eq!(g.pool.mode, RunMode::Dispatcher, "{}", case.name);
                }
                (Err(err), Err(want)) => {
                    let err = err.to_string();
                    assert!(err.contains(want), "{}: {err}", case.name);
                }
                (got, _) => panic!("{}: unexpected {got:?}", case.name),
            }
        }
    }
}
