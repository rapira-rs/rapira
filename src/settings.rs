use anyhow::{Context, bail};
use rapira_config::{
    ConfigCtx, LogSection, LogSettings, SupervisorSection, SupervisorSettings, resolve_log,
    resolve_supervisor,
};
use serde::Deserialize;
use std::path::Path;

/// The shape of `rapira.toml`: one table per plugin and the shared tables.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    http: Option<rapira_http::config::Section>,
    grpc: Option<rapira_grpc::config::Section>,
    #[serde(default)]
    supervisor: SupervisorSection,
    #[serde(default)]
    log: LogSection,
}

#[derive(Debug)]
pub struct Settings {
    pub http: Option<rapira_http::config::Settings>,
    pub grpc: Option<rapira_grpc::config::Settings>,
    pub supervisor: SupervisorSettings,
    pub log: LogSettings,
}

pub fn resolve(path: &Path) -> anyhow::Result<Settings> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let file: FileConfig =
        toml::from_str(&text).with_context(|| format!("parsing config file {}", path.display()))?;
    let ctx = ConfigCtx {
        dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
    };
    settings(file, &ctx)
}

fn settings(file: FileConfig, ctx: &ConfigCtx) -> anyhow::Result<Settings> {
    if file.http.is_none() && file.grpc.is_none() {
        bail!("no plugin configured: add an [http] or a [grpc] table");
    }
    let http = file
        .http
        .map(|section| rapira_http::config::resolve(section, ctx))
        .transpose()?;
    let grpc = file
        .grpc
        .map(|section| rapira_grpc::config::resolve(section, ctx))
        .transpose()?;
    let supervisor = resolve_supervisor(file.supervisor, ctx)?;
    let log = resolve_log(file.log)?;

    Ok(Settings {
        http,
        grpc,
        supervisor,
        log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_config::{LogLevel, resolve_pool};
    use std::path::PathBuf;

    fn ctx() -> ConfigCtx {
        ConfigCtx {
            dir: PathBuf::from("/w"),
        }
    }

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
                name: "fpm pm table",
                toml: "[pm]\nmode = \"static\"\n",
                error: Some("unknown field `pm`"),
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

    /// The e2e harness writes `[http.pool]` before `[http]`. TOML allows the super-table later.
    #[test]
    fn subtable_before_supertable_parses() {
        let file: FileConfig = toml::from_str(
            "[http.pool]\nentrypoint = \"a.php\"\nprocesses = 3\n\
             [log]\nlevel = \"debug\"\n\
             [http]\nlisten = \"127.0.0.1:7000\"\nmiddleware = [\"static\"]\n\
             [http.static]\nroot = \"public\"\n",
        )
        .unwrap();
        assert_eq!(resolve_log(file.log).unwrap().level, LogLevel::Debug);
        let http = file.http.unwrap();
        assert_eq!(http.listen.as_deref(), Some("127.0.0.1:7000"));
        assert_eq!(http.middleware, ["static"]);
        assert_eq!(
            http.r#static.and_then(|s| s.root).as_deref(),
            Some("public")
        );
        let pool = resolve_pool(http.pool, "http.pool", &ctx()).unwrap();
        assert_eq!(pool.processes, 3);
    }

    #[test]
    fn a_file_without_a_plugin_table_is_refused() {
        let file: FileConfig = toml::from_str("[log]\nlevel = \"info\"\n").unwrap();
        let err = settings(file, &ctx()).unwrap_err().to_string();
        assert_eq!(err, "no plugin configured: add an [http] or a [grpc] table");
    }
}
