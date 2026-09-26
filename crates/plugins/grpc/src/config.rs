use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use rapira_config::{
    ConfigCtx, ListenAddr, Mode, PoolSection, PoolSettings, check_entrypoint, nonzero_timeout,
    parse_listen, resolve_pool,
};
use serde::Deserialize;

use crate::{Config, Interceptor, Schema, Server};

/// The `[grpc]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub listen: Option<String>,
    pub descriptor_set: Option<String>,
    pub services: Option<Vec<String>>,
    pub reflection: Option<bool>,
    pub default_timeout_secs: Option<u64>,
    pub max_timeout_secs: Option<u64>,
    #[serde(default)]
    pub interceptors: Vec<String>,
    #[serde(default)]
    pub pool: PoolSection,
}

#[derive(Debug)]
pub struct Settings {
    pub listen: ListenAddr,
    pub descriptor_set: PathBuf,
    /// The services to serve out of the set; None serves the services of the files that no other file of the set imports.
    pub services: Option<Vec<String>>,
    pub reflection: bool,
    /// Deadline for a call that sends no timeout.
    pub default_timeout: Option<Duration>,
    /// Upper bound on the deadline a client asks for.
    pub max_timeout: Option<Duration>,
    /// `[grpc].interceptors` in list order.
    pub interceptors: Vec<Interceptor>,
    pub pool: PoolSettings,
}

/// Boot checks run here: entrypoint file.
pub fn resolve(section: Section, ctx: &ConfigCtx) -> Result<Settings> {
    let settings = settings(section, ctx)?;
    check_entrypoint("grpc.pool", &settings.pool.entrypoint)?;
    Ok(settings)
}

/// The settings of `section`. Reads no file.
fn settings(section: Section, ctx: &ConfigCtx) -> Result<Settings> {
    // No interceptor ships, so every name is unknown.
    if let Some(name) = section.interceptors.first() {
        bail!("grpc.interceptors: unknown interceptor \"{name}\"");
    }

    let listen = parse_listen(
        "grpc",
        section.listen.as_deref(),
        ListenAddr::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 50051))),
    )?;

    let Some(ds) = section.descriptor_set.as_deref().filter(|s| !s.is_empty()) else {
        bail!("grpc.descriptor_set is required");
    };
    let descriptor_set = ctx.resolve_path(ds)?;

    let services = section.services;
    if services.as_ref().is_some_and(Vec::is_empty) {
        bail!(
            "grpc.services must name at least one service; leave the key out to serve the services of the files that no other file imports"
        );
    }

    let default_timeout = optional_timeout("default_timeout_secs", section.default_timeout_secs)?;
    let max_timeout = optional_timeout("max_timeout_secs", section.max_timeout_secs)?;
    // The deadline policy returns the default without a clamp to the max.
    if let (Some(default), Some(max)) = (section.default_timeout_secs, section.max_timeout_secs)
        && default > max
    {
        bail!("grpc.default_timeout_secs ({default}) exceeds grpc.max_timeout_secs ({max})");
    }

    let pool = resolve_pool(section.pool, "grpc.pool", ctx)?;
    if pool.mode != Mode::Dispatcher {
        bail!(
            "grpc.pool.mode must be \"dispatcher\" (got \"{}\")",
            pool.mode
        );
    }

    Ok(Settings {
        listen,
        descriptor_set,
        services,
        reflection: section.reflection.unwrap_or(false),
        default_timeout,
        max_timeout,
        interceptors: Vec::new(),
        pool,
    })
}

fn optional_timeout(key: &str, secs: Option<u64>) -> Result<Option<Duration>> {
    secs.map(|secs| nonzero_timeout("grpc", key, secs))
        .transpose()
}

impl Server {
    /// The schema loads here, in the master, so a bad descriptor set or service name stops the boot before the fork.
    pub fn from_settings(settings: Settings) -> Result<Self> {
        let schema = Arc::new(Schema::load(
            &settings.descriptor_set,
            settings.services.as_deref(),
        )?);
        Ok(Self::init(Config {
            listen: settings.listen,
            schema,
            reflection: settings.reflection,
            default_timeout: settings.default_timeout,
            max_timeout: settings.max_timeout,
            keepalive_interval: Duration::from_secs(10),
            keepalive_timeout: Duration::from_secs(10),
            interceptors: settings.interceptors,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn ctx() -> ConfigCtx {
        ConfigCtx {
            dir: PathBuf::from("/w"),
        }
    }

    struct Want {
        listen: ListenAddr,
        descriptor_set: &'static str,
        services: Option<&'static [&'static str]>,
        reflection: bool,
        default_timeout: Option<Duration>,
        max_timeout: Option<Duration>,
    }

    struct Case {
        name: &'static str,
        toml: String,
        expected: Result<Want, &'static str>,
    }

    /// A `[grpc]` table with the required key, `extra` lines in the table, and a dispatcher pool.
    fn toml(extra: &str) -> String {
        format!(
            "descriptor_set = \"a.binpb\"\n{extra}\
             [pool]\nentrypoint = \"g.php\"\n"
        )
    }

    /// What `toml("")` resolves to.
    fn base() -> Want {
        Want {
            listen: ListenAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], 50051))),
            descriptor_set: "/w/a.binpb",
            services: None,
            reflection: false,
            default_timeout: None,
            max_timeout: None,
        }
    }

    /// Every resolved grpc pool runs in dispatcher mode, so a success case does not list the mode.
    #[test]
    fn grpc_table_resolves_and_validates() {
        let cases = [
            Case {
                name: "grpc only",
                toml: toml(""),
                expected: Ok(base()),
            },
            Case {
                name: "services narrow the set",
                toml: toml("services = [\"p.S\"]\n"),
                expected: Ok(Want {
                    services: Some(&["p.S"]),
                    ..base()
                }),
            },
            Case {
                name: "reflection on",
                toml: toml("reflection = true\n"),
                expected: Ok(Want {
                    reflection: true,
                    ..base()
                }),
            },
            Case {
                name: "descriptor_set missing",
                toml: "[pool]\nentrypoint = \"g.php\"\n".into(),
                expected: Err("grpc.descriptor_set is required"),
            },
            Case {
                name: "services empty",
                toml: toml("services = []\n"),
                expected: Err("grpc.services must name at least one service; leave the key out"),
            },
            Case {
                name: "worker mode refused",
                toml: toml("") + "mode = \"worker\"\n",
                expected: Err("grpc.pool.mode must be \"dispatcher\" (got \"worker\")"),
            },
            Case {
                name: "bad listen",
                toml: toml("listen = \"x\"\n"),
                expected: Err("invalid grpc.listen `x`"),
            },
            Case {
                name: "zero default timeout",
                toml: toml("default_timeout_secs = 0\n"),
                expected: Err("grpc.default_timeout_secs must be at least 1"),
            },
            Case {
                name: "max timeout above the cap",
                toml: toml("max_timeout_secs = 100000\n"),
                expected: Err("grpc.max_timeout_secs 100000 is too large (max 86400)"),
            },
            Case {
                name: "default above max",
                toml: toml("default_timeout_secs = 60\nmax_timeout_secs = 10\n"),
                expected: Err("grpc.default_timeout_secs (60) exceeds grpc.max_timeout_secs (10)"),
            },
            Case {
                name: "timeouts resolve",
                toml: toml("default_timeout_secs = 5\nmax_timeout_secs = 30\n"),
                expected: Ok(Want {
                    default_timeout: Some(Duration::from_secs(5)),
                    max_timeout: Some(Duration::from_secs(30)),
                    ..base()
                }),
            },
            Case {
                name: "unix listen",
                toml: toml("listen = \"unix:/run/g.sock\"\n"),
                expected: Ok(Want {
                    listen: ListenAddr::Unix(PathBuf::from("/run/g.sock")),
                    ..base()
                }),
            },
            Case {
                name: "unknown key",
                toml: toml("foo = 1\n"),
                expected: Err("unknown field `foo`"),
            },
        ];
        for case in cases {
            let got = toml::from_str::<Section>(&case.toml)
                .map_err(anyhow::Error::from)
                .and_then(|section| settings(section, &ctx()));
            match (got, case.expected) {
                (Ok(g), Ok(want)) => {
                    assert_eq!(g.listen, want.listen, "{}", case.name);
                    assert_eq!(
                        g.descriptor_set,
                        Path::new(want.descriptor_set),
                        "{}",
                        case.name
                    );
                    assert_eq!(
                        g.services
                            .as_deref()
                            .map(|s| s.iter().map(String::as_str).collect::<Vec<_>>()),
                        want.services.map(<[&str]>::to_vec),
                        "{}",
                        case.name
                    );
                    assert_eq!(g.reflection, want.reflection, "{}", case.name);
                    assert_eq!(g.default_timeout, want.default_timeout, "{}", case.name);
                    assert_eq!(g.max_timeout, want.max_timeout, "{}", case.name);
                    assert_eq!(g.pool.mode, Mode::Dispatcher, "{}", case.name);
                }
                (Err(err), Err(want)) => {
                    let err = err.to_string();
                    assert!(err.contains(want), "{}: {err}", case.name);
                }
                (got, _) => panic!("{}: unexpected {got:?}", case.name),
            }
        }
    }

    #[test]
    fn any_interceptor_name_fails_the_boot_because_none_ships() {
        let section: Section = toml::from_str("interceptors = [\"auth\"]\ndescriptor_set = \"echo.binpb\"\n[pool]\nentrypoint = \"w.php\"").unwrap();
        let err = resolve(section, &ctx()).unwrap_err().to_string();
        assert_eq!(err, "grpc.interceptors: unknown interceptor \"auth\"");
    }
}
