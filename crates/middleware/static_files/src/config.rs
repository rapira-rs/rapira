use anyhow::bail;
use rapira_config::ConfigCtx;
use serde::Deserialize;
use std::path::PathBuf;

/// The `[http.static]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub root: Option<String>,
    pub forbid: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct Settings {
    pub root: PathBuf,
    /// Extensions the middleware never serves from the root, with a leading dot.
    /// The middleware normalizes the case.
    pub forbid: Vec<String>,
}

pub fn resolve(section: Section, ctx: &ConfigCtx) -> anyhow::Result<Settings> {
    let root = match section.root.filter(|r| !r.is_empty()) {
        Some(r) => ctx.resolve_path(&r)?,
        None => bail!("http.static.root is required"),
    };
    let forbid = section.forbid.unwrap_or_else(|| vec![".php".to_owned()]);
    for entry in &forbid {
        // A separator or whitespace can never suffix-match a file name. Such an entry would
        // silently make the guard useless.
        if entry.len() < 2
            || !entry.starts_with('.')
            || entry.contains('/')
            || entry.chars().any(char::is_whitespace)
        {
            bail!("http.static.forbid entries must be extensions with a leading dot (`{entry}`)");
        }
    }
    Ok(Settings { root, forbid })
}
