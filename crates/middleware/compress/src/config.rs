use anyhow::bail;
use serde::Deserialize;

/// The `[http.compress]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub min_size: Option<u64>,
    pub gzip_level: Option<u32>,
    pub zstd_level: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Minimum known body size in bytes to compress. Streaming responses with unknown length compress.
    pub min_size: u64,
    /// Gzip compression level (1..=9).
    pub gzip_level: u32,
    /// Zstd compression level (1..=19).
    pub zstd_level: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            min_size: 1024,
            gzip_level: 4,
            zstd_level: 3,
        }
    }
}

pub fn resolve(section: Section) -> anyhow::Result<Settings> {
    let defaults = Settings::default();
    let min_size = section.min_size.unwrap_or(defaults.min_size);
    let gzip_level = section.gzip_level.unwrap_or(defaults.gzip_level);
    let zstd_level = section.zstd_level.unwrap_or(defaults.zstd_level);

    if !(1..=9).contains(&gzip_level) {
        bail!("http.compress.gzip_level must be between 1 and 9 (got {gzip_level})");
    }
    if !(1..=19).contains(&zstd_level) {
        bail!("http.compress.zstd_level must be between 1 and 19 (got {zstd_level})");
    }

    Ok(Settings {
        min_size,
        gzip_level,
        zstd_level,
    })
}
