use std::time::Duration;

use anyhow::{Context, Result};
use rapira_net::ListenAddr;
use serde::Deserialize;

/// The `[metrics]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub listen: String,
    /// Idle keep-alive connections close after this many seconds. 60 by default, as in [http].
    pub keepalive_timeout_secs: Option<u64>,
}

#[derive(Debug)]
pub struct Settings {
    pub listen: ListenAddr,
    pub keepalive_timeout: Duration,
}

pub fn resolve(section: Section) -> Result<Settings> {
    let listen = section
        .listen
        .parse::<ListenAddr>()
        .with_context(|| format!("invalid metrics.listen `{}`", section.listen))?;
    let keepalive_timeout = rapira_config::nonzero_timeout(
        "metrics",
        "keepalive_timeout_secs",
        section.keepalive_timeout_secs.unwrap_or(60),
    )?;
    Ok(Settings {
        listen,
        keepalive_timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        section: Section,
        error: &'static str,
    }

    #[test]
    fn bad_values_name_the_key() {
        let cases = [
            Case {
                name: "a bad address names the key",
                section: Section {
                    listen: "localhost".to_owned(),
                    keepalive_timeout_secs: None,
                },
                error: "invalid metrics.listen `localhost`: `localhost` is not a listen address: use host:port, :port, or unix:<path>",
            },
            Case {
                name: "a keep-alive of 0",
                section: Section {
                    listen: "127.0.0.1:9180".to_owned(),
                    keepalive_timeout_secs: Some(0),
                },
                error: "metrics.keepalive_timeout_secs must be at least 1",
            },
            Case {
                name: "a keep-alive above the cap",
                section: Section {
                    listen: "127.0.0.1:9180".to_owned(),
                    keepalive_timeout_secs: Some(100_000),
                },
                error: "metrics.keepalive_timeout_secs 100000 is too large (max 86400)",
            },
        ];
        for case in cases {
            let err = resolve(case.section).unwrap_err();
            assert_eq!(format!("{err:#}"), case.error, "{}", case.name);
        }
    }
}
