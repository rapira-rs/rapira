use std::path::PathBuf;

use anyhow::bail;
use rapira_config::ConfigCtx;
use serde::Deserialize;

/// The `[grpc.auth]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub tokens_file: Option<String>,
}

#[derive(Debug)]
pub struct Settings {
    /// One bearer token per line.
    pub tokens_file: PathBuf,
}

pub fn resolve(section: Section, ctx: &ConfigCtx) -> anyhow::Result<Settings> {
    match section.tokens_file.filter(|f| !f.is_empty()) {
        Some(f) => Ok(Settings {
            tokens_file: ctx.resolve_path(&f)?,
        }),
        None => bail!("grpc.auth.tokens_file is required"),
    }
}
