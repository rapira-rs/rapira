use anyhow::{Context, bail};
use rapira_config::{
    LogSection, LogSettings, SupervisorSection, SupervisorSettings, resolve_log, resolve_supervisor,
};
use serde::Deserialize;
use std::path::Path;

/// The shape of `rapira.toml`: one table per plugin and the shared tables.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    http: Option<rapira_http::config::Section>,
    grpc: Option<rapira_grpc::config::Section>,
    observability: Option<rapira_observability::config::Section>,
    #[serde(default)]
    supervisor: SupervisorSection,
    #[serde(default)]
    log: LogSection,
}

#[derive(Debug)]
pub struct Settings {
    pub http: Option<rapira_http::config::Settings>,
    pub grpc: Option<rapira_grpc::config::Settings>,
    pub observability: Option<rapira_observability::config::Settings>,
    pub supervisor: SupervisorSettings,
    pub log: LogSettings,
}

pub fn resolve(path: &Path) -> anyhow::Result<Settings> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let file: FileConfig =
        toml::from_str(&text).with_context(|| format!("parsing config file {}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    settings(file, dir)
}

fn settings(file: FileConfig, dir: &Path) -> anyhow::Result<Settings> {
    if file.http.is_none() && file.grpc.is_none() {
        bail!("no plugin configured: add an [http] or a [grpc] table");
    }
    let http = file
        .http
        .map(|section| rapira_http::config::resolve(section, dir))
        .transpose()?;
    let grpc = file
        .grpc
        .map(|section| rapira_grpc::config::resolve(section, dir))
        .transpose()?;
    let observability = file
        .observability
        .map(rapira_observability::config::resolve)
        .transpose()?;
    let supervisor = resolve_supervisor(file.supervisor, dir)?;
    let log = resolve_log(file.log)?;

    Ok(Settings {
        http,
        grpc,
        observability,
        supervisor,
        log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        toml: &'static str,
        /// None: the file parses.
        error: Option<&'static str>,
    }

    /// The pool belongs to its plugin. A top-level table names no plugin, so it must fail at parse time.
    #[test]
    fn file_tables_parse() {
        let cases = [
            Case {
                name: "top-level pool table",
                toml: "[pool]\nentrypoint = \"a.php\"\n",
                error: Some("unknown field `pool`"),
            },
            Case {
                name: "unknown table",
                toml: "[nope]\nx = 1\n",
                error: Some("unknown field `nope`"),
            },
            Case {
                name: "pool under http",
                toml: "[http.pool]\nentrypoint = \"a.php\"\n",
                error: None,
            },
            Case {
                name: "both plugin tables",
                toml: "[http.pool]\nentrypoint = \"a.php\"\n\
                       [grpc]\ndescriptor_set = \"a.binpb\"\n[grpc.pool]\nentrypoint = \"g.php\"\n",
                error: None,
            },
            Case {
                name: "observability table",
                toml: "[http.pool]\nentrypoint = \"a.php\"\n[observability]\nlisten = \"127.0.0.1:9180\"\n[observability.metrics]\n",
                error: None,
            },
            Case {
                name: "probes table",
                toml: "[http.pool]\nentrypoint = \"a.php\"\n[observability]\nlisten = \"127.0.0.1:9180\"\n[observability.probes]\n",
                error: None,
            },
            Case {
                name: "observability keep-alive key",
                toml: "[observability]\nlisten = \":9180\"\nkeepalive_timeout_secs = 5\n[observability.metrics]\n",
                error: None,
            },
            Case {
                name: "observability table without listen",
                toml: "[observability]\n",
                error: Some("missing field `listen`"),
            },
            Case {
                name: "unknown key in the observability table",
                toml: "[observability]\nlisten = \":9180\"\npath = \"/m\"\n[observability.metrics]\n",
                error: Some("unknown field `path`"),
            },
            Case {
                name: "unknown key in the metrics sub-table",
                toml: "[observability]\nlisten = \":9180\"\n[observability.metrics]\npath = \"/m\"\n",
                error: Some("unknown field `path`"),
            },
            Case {
                name: "unknown key in the probes sub-table",
                toml: "[observability]\nlisten = \":9180\"\n[observability.probes]\npath = \"/p\"\n",
                error: Some("unknown field `path`"),
            },
            Case {
                name: "shipped example",
                toml: include_str!("../examples/rapira.toml"),
                error: None,
            },
        ];
        for case in cases {
            let got = toml::from_str::<FileConfig>(case.toml);
            match (got, case.error) {
                (Ok(_), None) => {}
                (Err(err), Some(want)) => {
                    let err = err.to_string();
                    assert!(err.contains(want), "{}: {err}", case.name);
                }
                (got, _) => panic!("{}: unexpected {got:?}", case.name),
            }
        }
    }

    #[test]
    fn a_file_without_a_plugin_table_is_refused() {
        struct Case {
            name: &'static str,
            toml: &'static str,
        }
        let cases = [
            Case {
                name: "log table only",
                toml: "[log]\nlevel = \"info\"\n",
            },
            Case {
                name: "observability table only",
                toml: "[observability]\nlisten = \"127.0.0.1:9180\"\n[observability.metrics]\n",
            },
        ];
        for case in cases {
            let file: FileConfig = toml::from_str(case.toml).unwrap();
            let err = settings(file, Path::new("/w")).unwrap_err().to_string();
            assert_eq!(
                err, "no plugin configured: add an [http] or a [grpc] table",
                "{}",
                case.name
            );
        }
    }
}
