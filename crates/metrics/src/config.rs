use anyhow::{Context, Result};
use rapira_net::ListenAddr;
use serde::Deserialize;

/// The `[metrics]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub listen: String,
}

#[derive(Debug)]
pub struct Settings {
    pub listen: ListenAddr,
}

pub fn resolve(section: Section) -> Result<Settings> {
    let listen = section
        .listen
        .parse::<ListenAddr>()
        .with_context(|| format!("invalid metrics.listen `{}`", section.listen))?;
    Ok(Settings { listen })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_address_names_the_key() {
        let err = resolve(Section {
            listen: "localhost".to_owned(),
        })
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "invalid metrics.listen `localhost`: `localhost` is not a listen address: use host:port, :port, or unix:<path>"
        );
    }
}
