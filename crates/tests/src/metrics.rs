//! A reader for the Prometheus text format of the `[observability.metrics]` endpoint.

use std::collections::BTreeMap;

/// The samples of `text`, keyed by the series as the endpoint writes it, for example `rapira_workers{pool="http",state="idle"}`. Comment lines are skipped.
pub fn samples(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (series, value) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("no value in {line:?}"));
            let value = value
                .parse()
                .unwrap_or_else(|_| panic!("no integer value in {line:?}"));
            (series.to_owned(), value)
        })
        .collect()
}
