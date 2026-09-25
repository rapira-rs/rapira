use anyhow::bail;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::config_relative;
use crate::listen::Listen;
use crate::pool::{PoolSection, PoolSettings};

#[derive(Debug)]
pub struct HttpSettings {
    pub listen: Listen,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    pub write_timeout: std::time::Duration,
    pub keepalive_timeout: std::time::Duration,
    pub unsafe_field_names: UnsafeFieldNames,
    pub uploads: UploadSettings,
    /// sendFile() containment root; falls back to the entrypoint directory.
    pub sendfile_root: PathBuf,
    // [http].middleware list order
    pub middleware: Vec<MiddlewareSettings>,
    pub pool: PoolSettings,
}

#[derive(Debug)]
pub struct StaticSettings {
    pub root: PathBuf,
    /// Extensions the middleware never serves from the root, with a leading dot.
    /// The middleware normalizes the case.
    pub forbid: Vec<String>,
}

#[derive(Debug)]
pub struct UploadSettings {
    pub dir: PathBuf,
    pub max_file_size: u64,
    pub max_field_size: usize,
    pub max_files: usize,
    pub max_parts: usize,
    pub max_part_headers: usize,
}

/// The `HTTP_*` mapping rewrites `-` to `_` and PHP rewrites `.` to `_`, so `X_Forwarded_For` and `X.Forwarded.For` both land on `HTTP_X_FORWARDED_FOR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnsafeFieldNames {
    #[default]
    Drop,
    Reject,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpSection {
    pub(crate) listen: Option<String>,
    pub(crate) server_name: Option<String>,
    pub(crate) server_port: Option<u16>,
    pub(crate) max_body_size_mb: Option<usize>,
    pub(crate) write_timeout_secs: Option<u64>,
    pub(crate) keepalive_timeout_secs: Option<u64>,
    pub(crate) unsafe_field_names: Option<UnsafeFieldNames>,
    pub(crate) uploads: Option<UploadsSection>,
    #[serde(default)]
    pub(crate) sendfile: SendfileSection,
    pub(crate) middleware: Option<Vec<String>>,
    pub(crate) r#static: Option<StaticSection>,
    #[serde(default)]
    pub(crate) pool: PoolSection,
}

// ------------ middleware stuff ---------------
// add new middleware here, with settings if needed
#[derive(Debug)]
pub enum MiddlewareSettings {
    Static(StaticSettings),
}
// ---------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StaticSection {
    pub(crate) root: Option<String>,
    pub(crate) forbid: Option<Vec<String>>,
}

pub(crate) fn resolve_static(
    section: StaticSection,
    config_dir: Option<&Path>,
) -> anyhow::Result<StaticSettings> {
    let root = match section.root.filter(|r| !r.is_empty()) {
        Some(r) => config_relative(config_dir, &r)?,
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
    Ok(StaticSettings { root, forbid })
}

// resolve all existing vs requested middlewares
// TODO: think about some container, like endure in RR
pub(crate) fn resolve_middleware(
    list: Option<Vec<String>>,
    mut static_files: Option<StaticSettings>,
) -> anyhow::Result<Vec<MiddlewareSettings>> {
    let list = list.unwrap_or_default();

    for (i, name) in list.iter().enumerate() {
        if list[..i].contains(name) {
            bail!("http.middleware lists \"{name}\" twice")
        }
    }

    let mut middleware: Vec<MiddlewareSettings> = Vec::new();
    for name in &list {
        match name.as_str() {
            // check new middleware here
            "static" => match static_files.take() {
                Some(settings) => middleware.push(MiddlewareSettings::Static(settings)),
                None => {
                    bail!("http.middleware lists \"static\" but [http.static] is missing")
                }
            },
            other => {
                bail!("http.middleware entry \"{other}\" is unknown; known middleware: \"static\"")
            }
        }
    }

    // check for the configured, but not listed
    // TODO: worth double checking
    // some people requested some kind of enabled=false/true
    if static_files.is_some() {
        bail!("[http.static] is configured but http.middleware does not list \"static\"");
    }

    Ok(middleware)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendfileSection {
    pub(crate) root: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UploadsSection {
    dir: Option<String>,
    max_file_size_mb: Option<u64>,
    max_field_size_kb: Option<usize>,
    max_files: Option<usize>,
    max_parts: Option<usize>,
    max_part_headers: Option<usize>,
}

pub(crate) fn resolve_uploads(
    section: UploadsSection,
    config_dir: Option<&Path>,
) -> anyhow::Result<UploadSettings> {
    let dir = match section.dir.filter(|d| !d.is_empty()) {
        Some(d) => config_relative(config_dir, &d)?,
        None => std::env::temp_dir(),
    };
    let max_file_size_mb = section.max_file_size_mb.unwrap_or(2);
    if max_file_size_mb == 0 {
        bail!("http.uploads.max_file_size_mb must be at least 1");
    }
    let max_file_size = max_file_size_mb.checked_mul(1024 * 1024).ok_or_else(|| {
        anyhow::anyhow!("http.uploads.max_file_size_mb {max_file_size_mb} is too large")
    })?;
    let max_field_size_kb = section.max_field_size_kb.unwrap_or(256);
    if max_field_size_kb == 0 {
        bail!("http.uploads.max_field_size_kb must be at least 1");
    }
    let max_field_size = max_field_size_kb.checked_mul(1024).ok_or_else(|| {
        anyhow::anyhow!("http.uploads.max_field_size_kb {max_field_size_kb} is too large")
    })?;
    let max_parts = section.max_parts.unwrap_or(1024);
    if max_parts == 0 {
        bail!("http.uploads.max_parts must be at least 1");
    }
    let max_part_headers = section.max_part_headers.unwrap_or(32);
    if max_part_headers == 0 {
        bail!("http.uploads.max_part_headers must be at least 1");
    }
    let max_files = section.max_files.unwrap_or(20);
    if max_files == 0 {
        bail!("http.uploads.max_files must be at least 1");
    }
    Ok(UploadSettings {
        dir,
        max_file_size,
        max_field_size,
        max_files,
        max_parts,
        max_part_headers,
    })
}
