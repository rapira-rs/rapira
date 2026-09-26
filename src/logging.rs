use rapira_config::{LogFormat, LogSettings};
use std::io::{self, IsTerminal};
use tracing_subscriber::fmt::time::ChronoUtc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub fn init(log: &LogSettings) {
    let filter = build_filter(std::env::var("RUST_LOG").ok().as_deref(), log);
    let ansi = ansi_enabled(
        io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    );
    tracing_subscriber::registry()
        .with(filter)
        .with(make_layer(log.format, ansi))
        .init();
}

fn ansi_enabled(stderr_is_tty: bool, no_color: Option<&std::ffi::OsStr>) -> bool {
    stderr_is_tty && no_color.is_none_or(|v| v.is_empty())
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

fn make_layer<S>(format: LogFormat, ansi: bool) -> Box<dyn Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_timer(ChronoUtc::new("%Y-%m-%dT%H:%M:%S%.3fZ".into()))
            .with_writer(io::stderr)
            .boxed(),
        LogFormat::Plain => tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_writer(io::stderr)
            .boxed(),
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

    #[test]
    fn no_color_counts_as_set_only_when_non_empty() {
        use std::ffi::OsStr;
        assert!(ansi_enabled(true, None));
        assert!(
            ansi_enabled(true, Some(OsStr::new(""))),
            "empty NO_COLOR counts as unset"
        );
        assert!(!ansi_enabled(true, Some(OsStr::new("1"))));
        assert!(!ansi_enabled(false, None), "never color a non-tty");
    }
}
