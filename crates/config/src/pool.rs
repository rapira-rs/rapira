use anyhow::bail;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{capped_timeout, config_relative};

#[derive(Debug)]
pub struct PoolSettings {
    pub entrypoint: PathBuf,
    pub processes: usize,
    pub mode: RunMode,
    pub scaling: Scaling,
    /// Requests a worker serves before recycling itself (with jitter); 0 = unlimited.
    pub max_requests: u64,
    pub process_idle_timeout: Duration,
    /// Wall-clock bound on a single request; zero = disabled.
    pub request_terminate_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    Static,
    Dynamic { min_spare: usize, max_spare: usize },
    Ondemand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Classic,
    Worker,
    #[default]
    Dispatcher,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::Classic => "classic",
            RunMode::Worker => "worker",
            RunMode::Dispatcher => "dispatcher",
        }
    }
}

/// Embedded by name: serde does not support `#[serde(flatten)]` alongside `deny_unknown_fields`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PoolSection {
    entrypoint: Option<String>,
    processes: Option<usize>,
    mode: Option<RunMode>,
    scaling: Option<ScalingKey>,
    min_spare: Option<usize>,
    max_spare: Option<usize>,
    max_requests: Option<u64>,
    process_idle_timeout_secs: Option<u64>,
    request_terminate_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ScalingKey {
    Static,
    Dynamic,
    Ondemand,
}

fn default_processes() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// `table` is the qualified table of the calling plugin, such as `http.pool`. Every message carries it.
pub(crate) fn resolve_pool(
    section: PoolSection,
    table: &str,
    config_dir: Option<&Path>,
) -> anyhow::Result<PoolSettings> {
    let processes = section.processes.unwrap_or_else(default_processes);
    if processes == 0 {
        bail!("{table}.processes must be at least 1");
    }

    let mode = section.mode.unwrap_or_default();

    let Some(ep) = section.entrypoint.as_deref().filter(|s| !s.is_empty()) else {
        bail!("{table}.entrypoint is required");
    };
    let entrypoint = config_relative(config_dir, ep)?;

    let scaling = match section.scaling.unwrap_or(ScalingKey::Static) {
        ScalingKey::Dynamic => {
            let (Some(min_spare), Some(max_spare)) = (section.min_spare, section.max_spare) else {
                bail!(
                    "{table}.scaling = \"dynamic\" requires {table}.min_spare and {table}.max_spare"
                );
            };
            if !(1..=max_spare).contains(&min_spare) || max_spare > processes {
                bail!(
                    "{table} spares must satisfy 1 <= min_spare ({min_spare}) <= max_spare ({max_spare}) <= {table}.processes ({processes})"
                );
            }
            Scaling::Dynamic {
                min_spare,
                max_spare,
            }
        }
        other => {
            if section.min_spare.is_some() || section.max_spare.is_some() {
                bail!(
                    "{table}.min_spare/{table}.max_spare are only valid with {table}.scaling = \"dynamic\""
                );
            }
            if other == ScalingKey::Static {
                Scaling::Static
            } else {
                Scaling::Ondemand
            }
        }
    };

    Ok(PoolSettings {
        entrypoint,
        processes,
        mode,
        scaling,
        max_requests: section.max_requests.unwrap_or(0),
        process_idle_timeout: capped_timeout(
            table,
            "process_idle_timeout_secs",
            section.process_idle_timeout_secs.unwrap_or(10),
        )?,
        request_terminate_timeout: capped_timeout(
            table,
            "request_terminate_timeout_secs",
            section.request_terminate_timeout_secs.unwrap_or(0),
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every message carries the caller's table, so a second pool reports its own keys.
    #[test]
    fn resolve_pool_prefixes_errors_with_the_table() {
        let err = resolve_pool(PoolSection::default(), "grpc.pool", None)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "grpc.pool.entrypoint is required");
    }
}
