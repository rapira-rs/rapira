use anyhow::bail;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    #[default]
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Plain,
    Json,
}

#[derive(Debug)]
pub struct LogSettings {
    pub level: LogLevel,
    pub format: LogFormat,
    /// Keys match by prefix; BTreeMap keeps the rendered filter byte-stable.
    pub targets: BTreeMap<String, LogLevel>,
}

/// The `[log]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSection {
    level: Option<LogLevel>,
    format: Option<LogFormat>,
    /// Target names are free-form keys; `resolve_log` checks their shape.
    #[serde(default)]
    targets: BTreeMap<String, LogLevel>,
}

/// Target names are open-ended module paths, so keys are pinned to the shape EnvFilter parses as a plain target: anything else is filter grammar (`[`, `,`, `=`) and would be reinterpreted.
pub fn resolve_log(section: LogSection) -> anyhow::Result<LogSettings> {
    for name in section.targets.keys() {
        let mut chars = name.chars();
        let ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'));
        if !ok {
            bail!(
                "log.targets key `{}` is not a log target: use letters, digits and `_` `:` `.` `-`, starting with a letter, digit or `_`",
                name.escape_default()
            );
        }
    }

    Ok(LogSettings {
        level: section.level.unwrap_or_default(),
        format: section.format.unwrap_or_default(),
        targets: section.targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        toml: &'static str,
        error: &'static str,
    }

    /// The filter string is assembled from the target keys, so a key carrying filter syntax would inject directives (`"php=trace,tokio" = "debug"` reads as two).
    #[test]
    fn log_errors_name_the_key() {
        let cases = [
            Case {
                name: "unknown key",
                toml: "bogus = 1\n",
                error: "unknown field `bogus`",
            },
            Case {
                name: "unknown level",
                toml: "level = \"verbose\"\n",
                error: "unknown variant `verbose`",
            },
            Case {
                name: "unknown format",
                toml: "format = \"pretty\"\n",
                error: "unknown variant `pretty`",
            },
            Case {
                name: "empty target",
                toml: "[targets]\n\"\" = \"info\"\n",
                error: "log.targets key `` is not a log target",
            },
            Case {
                name: "target with filter directives",
                toml: "[targets]\n\"php=trace,tokio\" = \"info\"\n",
                error: "log.targets key `php=trace,tokio` is not a log target",
            },
            Case {
                name: "target with a space",
                toml: "[targets]\n\"a b\" = \"info\"\n",
                error: "log.targets key `a b` is not a log target",
            },
            Case {
                name: "target with a slash",
                toml: "[targets]\n\"a/b\" = \"info\"\n",
                error: "log.targets key `a/b` is not a log target",
            },
            Case {
                name: "target with a control character",
                toml: "[targets]\n\"a\\u001Bb\" = \"info\"\n",
                error: "log.targets key `a\\u{1b}b` is not a log target",
            },
            Case {
                name: "target with a span filter",
                toml: "[targets]\n\"http[request]\" = \"info\"\n",
                error: "log.targets key `http[request]` is not a log target",
            },
            Case {
                name: "target with a leading dot",
                toml: "[targets]\n\".php\" = \"info\"\n",
                error: "log.targets key `.php` is not a log target",
            },
        ];
        for case in cases {
            let err = toml::from_str::<LogSection>(case.toml)
                .map_err(anyhow::Error::from)
                .and_then(resolve_log)
                .expect_err(case.name)
                .to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }
}
