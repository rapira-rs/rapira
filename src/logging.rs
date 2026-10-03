use rapira_config::{LogFormat, LogSettings};
use std::io::{self, IsTerminal};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub fn init(log: &LogSettings) {
    let filter = build_filter(std::env::var("RUST_LOG").ok().as_deref(), log);
    let tty: bool = io::stderr().is_terminal();
    tracing_subscriber::registry()
        .with(filter)
        .with(make_layer(log.format, tty))
        .init();
}

fn build_filter(rust_log: Option<&str>, log: &LogSettings) -> EnvFilter {
    match rust_log {
        Some(s) if !s.trim().is_empty() => EnvFilter::new(s),
        _ => {
            let mut spec = log.level.as_str().to_owned();
            for (target, level) in &log.targets {
                spec += &format!(",{target}={}", level.as_str());
            }
            EnvFilter::builder()
                .parse(&spec)
                .expect("resolve_log validated every target")
        }
    }
}

fn make_layer<S>(format: LogFormat, tty: bool) -> Box<dyn Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(io::stderr)
            .boxed(),
        LogFormat::Plain => {
            // The default ANSI setting reads NO_COLOR, and with_ansi overrides it. https://docs.rs/tracing-subscriber/0.3.18/tracing_subscriber/fmt/struct.Layer.html#method.with_ansi
            let layer = tracing_subscriber::fmt::layer().with_writer(io::stderr);
            if tty {
                layer.boxed()
            } else {
                layer.with_ansi(false).boxed()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_config::LogLevel;
    use std::collections::BTreeMap;

    fn settings(level: LogLevel, targets: &[(&str, LogLevel)]) -> LogSettings {
        LogSettings {
            level,
            format: LogFormat::Plain,
            targets: targets
                .iter()
                .map(|(t, l)| (t.to_string(), *l))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn config_levels_are_valid_filter_directives() {
        for level in [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ] {
            let filter = build_filter(None, &settings(level, &[]));
            assert_eq!(filter.to_string(), level.as_str());
        }
    }
}
