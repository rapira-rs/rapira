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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogSection {
    level: Option<LogLevel>,
    format: Option<LogFormat>,
    /// Target names are free-form keys; `resolve_log` checks their shape.
    #[serde(default)]
    targets: BTreeMap<String, LogLevel>,
}

/// Target names are open-ended module paths, so keys are pinned to the shape EnvFilter parses as a plain target: anything else is filter grammar (`[`, `,`, `=`) and would be reinterpreted.
pub(crate) fn resolve_log(section: LogSection) -> anyhow::Result<LogSettings> {
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
