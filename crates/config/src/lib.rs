use anyhow::{Context, bail};
use std::path::PathBuf;
use std::time::Duration;

mod listen;
mod log;
mod pool;
mod supervisor;

pub use listen::ListenAddr;
pub use log::{LogFormat, LogLevel, LogSection, LogSettings, resolve_log};
pub use pool::{Mode, PoolSection, PoolSettings, Scaling, check_entrypoint, resolve_pool};
pub use supervisor::{SupervisorSection, SupervisorSettings, resolve_supervisor};

/// What every section resolves against.
#[derive(Debug, Clone)]
pub struct ConfigCtx {
    /// The directory of the config file. Relative paths in the file resolve against it.
    pub dir: PathBuf,
}

impl ConfigCtx {
    pub fn resolve_path(&self, value: &str) -> std::io::Result<PathBuf> {
        std::path::absolute(self.dir.join(value))
    }
}

/// `{table}.listen`, or `default` when the key is absent.
pub fn parse_listen(
    table: &str,
    raw: Option<&str>,
    default: ListenAddr,
) -> anyhow::Result<ListenAddr> {
    match raw {
        Some(s) => s
            .parse::<ListenAddr>()
            .with_context(|| format!("invalid {table}.listen `{s}`")),
        None => Ok(default),
    }
}

/// A `_secs` key that must be at least 1 and at most `MAX_TIMEOUT_SECS`.
pub fn nonzero_timeout(table: &str, key: &str, secs: u64) -> anyhow::Result<Duration> {
    if secs == 0 {
        bail!("{table}.{key} must be at least 1");
    }
    capped_timeout(table, key, secs)
}

/// Caps every `*_secs` key so the master's deadline arithmetic can't overflow.
const MAX_TIMEOUT_SECS: u64 = 86_400;

pub fn capped_timeout(table: &str, key: &str, secs: u64) -> anyhow::Result<Duration> {
    if secs > MAX_TIMEOUT_SECS {
        bail!("{table}.{key} {secs} is too large (max {MAX_TIMEOUT_SECS})");
    }
    Ok(Duration::from_secs(secs))
}
