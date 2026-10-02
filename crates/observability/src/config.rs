use std::time::Duration;

use anyhow::{Context, Result, bail};
use rapira_net::ListenAddr;
use serde::Deserialize;

/// The `[observability]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub listen: String,
    /// Idle keep-alive connections close after this many seconds. 60 by default, as in [http].
    pub keepalive_timeout_secs: Option<u64>,
    /// Turns on `GET /metrics`.
    pub metrics: Option<Endpoints>,
    /// Turns on `GET /livez` and `GET /readyz`.
    pub probes: Option<Endpoints>,
}

/// A sub-table with no keys.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoints {}

#[derive(Debug)]
pub struct Settings {
    pub listen: ListenAddr,
    pub keepalive_timeout: Duration,
    pub metrics: bool,
    pub probes: bool,
}

pub fn resolve(section: Section) -> Result<Settings> {
    let listen = section
        .listen
        .parse::<ListenAddr>()
        .with_context(|| format!("invalid observability.listen `{}`", section.listen))?;
    let keepalive_timeout = rapira_config::nonzero_timeout(
        "observability",
        "keepalive_timeout_secs",
        section.keepalive_timeout_secs.unwrap_or(60),
    )?;
    if section.metrics.is_none() && section.probes.is_none() {
        bail!("[observability] needs [observability.metrics] or [observability.probes]");
    }
    Ok(Settings {
        listen,
        keepalive_timeout,
        metrics: section.metrics.is_some(),
        probes: section.probes.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        section: Section,
        /// None: the section resolves.
        error: Option<&'static str>,
    }

    #[test]
    fn resolve_needs_valid_values_and_an_endpoint_table() {
        let cases = [
            Case {
                name: "a bad address names the key",
                section: Section {
                    listen: "localhost".to_owned(),
                    keepalive_timeout_secs: None,
                    metrics: Some(Endpoints {}),
                    probes: None,
                },
                error: Some(
                    "invalid observability.listen `localhost`: `localhost` is not a listen address: use host:port, :port, or unix:<path>",
                ),
            },
            Case {
                name: "a keep-alive of 0",
                section: Section {
                    listen: "127.0.0.1:9180".to_owned(),
                    keepalive_timeout_secs: Some(0),
                    metrics: Some(Endpoints {}),
                    probes: None,
                },
                error: Some("observability.keepalive_timeout_secs must be at least 1"),
            },
            Case {
                name: "a keep-alive above the cap",
                section: Section {
                    listen: "127.0.0.1:9180".to_owned(),
                    keepalive_timeout_secs: Some(100_000),
                    metrics: Some(Endpoints {}),
                    probes: None,
                },
                error: Some("observability.keepalive_timeout_secs 100000 is too large (max 86400)"),
            },
            Case {
                name: "no endpoint table",
                section: Section {
                    listen: "127.0.0.1:9180".to_owned(),
                    keepalive_timeout_secs: None,
                    metrics: None,
                    probes: None,
                },
                error: Some(
                    "[observability] needs [observability.metrics] or [observability.probes]",
                ),
            },
            Case {
                name: "probes only",
                section: Section {
                    listen: "127.0.0.1:9180".to_owned(),
                    keepalive_timeout_secs: None,
                    metrics: None,
                    probes: Some(Endpoints {}),
                },
                error: None,
            },
        ];
        for case in cases {
            match (resolve(case.section), case.error) {
                (Ok(_), None) => {}
                (Err(err), Some(want)) => assert_eq!(format!("{err:#}"), want, "{}", case.name),
                (got, _) => panic!("{}: unexpected {got:?}", case.name),
            }
        }
    }
}
