use anyhow::bail;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::listen::resolve_listen;
use crate::pool::{PoolSection, resolve_pool};
use crate::{Listen, PoolSettings, RunMode, config_relative};

#[derive(Debug)]
pub struct GrpcSettings {
    pub listen: Listen,
    pub protos: Vec<PathBuf>,
    pub import_paths: Vec<PathBuf>,
    pub reflection: bool,
    pub gzip_responses: bool,
    pub max_request_message_size: usize,
    pub max_response_message_size: usize,
    pub pool: PoolSettings,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrpcSection {
    listen: Option<String>,
    #[serde(default)]
    protos: Vec<String>,
    #[serde(default)]
    import_paths: Vec<String>,
    reflection: Option<bool>,
    #[serde(default)]
    interceptors: Vec<String>,
    #[serde(default)]
    compression: CompressionSection,
    max_request_message_size_mb: Option<usize>,
    max_response_message_size_mb: Option<usize>,
    #[serde(default)]
    pool: PoolSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompressionSection {
    #[serde(default)]
    gzip: GzipSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GzipSection {
    enabled: Option<bool>,
}

pub(crate) fn resolve_grpc(
    section: GrpcSection,
    config_dir: Option<&Path>,
) -> anyhow::Result<GrpcSettings> {
    let listen = resolve_listen(section.listen.as_deref(), "grpc", 9001, config_dir)?;
    if section.protos.is_empty() {
        bail!("grpc.protos must contain at least one directory");
    }
    let protos = resolve_paths(section.protos, "protos", config_dir)?;
    let import_paths = resolve_paths(section.import_paths, "import_paths", config_dir)?;
    let max_request_message_size = message_size(
        section.max_request_message_size_mb,
        "max_request_message_size_mb",
    )?;
    let max_response_message_size = message_size(
        section.max_response_message_size_mb,
        "max_response_message_size_mb",
    )?;
    if let Some(name) = section.interceptors.first() {
        match name.as_str() {
            "static" => bail!("grpc.interceptors entry \"static\" is not supported for gRPC"),
            _ => bail!("grpc.interceptors entry \"{name}\" is unknown"),
        }
    }
    let pool = resolve_pool(section.pool, "grpc.pool", config_dir)?;
    if pool.mode != RunMode::Dispatcher {
        bail!("grpc.pool.mode must be \"dispatcher\"");
    }
    Ok(GrpcSettings {
        listen,
        protos,
        import_paths,
        reflection: section.reflection.unwrap_or(true),
        gzip_responses: section.compression.gzip.enabled.unwrap_or(false),
        max_request_message_size,
        max_response_message_size,
        pool,
    })
}

fn resolve_paths(
    paths: Vec<String>,
    key: &str,
    config_dir: Option<&Path>,
) -> anyhow::Result<Vec<PathBuf>> {
    paths
        .into_iter()
        .map(|path| {
            if path.is_empty() {
                bail!("grpc.{key} entries must be nonempty directory paths");
            }
            Ok(config_relative(config_dir, &path)?)
        })
        .collect()
}

fn message_size(value: Option<usize>, key: &str) -> anyhow::Result<usize> {
    let mb = value.unwrap_or(4);
    if mb == 0 {
        bail!("grpc.{key} must be at least 1");
    }
    mb.checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("grpc.{key} {mb} is too large"))
}
