use rapira_config::{LogFormat, LogSettings};
use std::io::{self, IsTerminal};
use tracing_subscriber::fmt::time::ChronoUtc;
use tracing_subscriber::fmt::writer::MakeWriter;
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
        .with(make_layer(log.format, ansi, io::stderr))
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

fn make_layer<S, W>(format: LogFormat, ansi: bool, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_timer(ChronoUtc::new("%Y-%m-%dT%H:%M:%S%.3fZ".into()))
            .with_writer(writer)
            .boxed(),
        LogFormat::Plain => tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_writer(writer)
            .boxed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_config::LogLevel;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("utf8 log output")
        }
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

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

    /// Builds the subscriber the same way `init` does: filter plus plain uncolored layer.
    fn captured(filter: EnvFilter, emit: impl Fn()) -> String {
        let sink = Sink::default();
        let sub = tracing_subscriber::registry().with(filter).with(make_layer(
            LogFormat::Plain,
            false,
            sink.clone(),
        ));
        tracing::subscriber::with_default(sub, emit);
        sink.text()
    }

    fn emit_probe_events() {
        tracing::error!(target: "rapira", "rapira-error-mark");
        tracing::info!(target: "rapira", "rapira-info-mark");
        tracing::warn!(target: "php", "php-warn-mark");
        tracing::info!(target: "php", "php-info-mark");
        tracing::warn!(target: "php_sys", "php-scoped-warn-mark");
    }

    #[test]
    fn config_spec_filters_by_level_and_target_prefix() {
        let log = settings(LogLevel::Error, &[("php", LogLevel::Warn)]);
        let out = captured(build_filter(None, &log), emit_probe_events);
        assert!(out.contains("rapira-error-mark"));
        assert!(
            out.contains("php-warn-mark"),
            "target override lost:\n{out}"
        );
        assert!(
            out.contains("php-scoped-warn-mark"),
            "prefix match lost:\n{out}"
        );
        assert!(!out.contains("rapira-info-mark"));
        assert!(!out.contains("php-info-mark"));
    }

    #[test]
    fn rust_log_replaces_the_config_spec_wholesale() {
        let log = settings(LogLevel::Error, &[("php", LogLevel::Warn)]);
        let out = captured(build_filter(Some("info"), &log), emit_probe_events);
        assert!(
            out.contains("php-info-mark"),
            "config directive survived:\n{out}"
        );
        assert!(out.contains("rapira-info-mark"));
    }

    #[test]
    fn blank_rust_log_falls_back_to_the_config_spec() {
        let log = settings(LogLevel::Error, &[("php", LogLevel::Warn)]);
        let out = captured(build_filter(Some("  "), &log), emit_probe_events);
        assert!(out.contains("php-warn-mark"));
        assert!(!out.contains("php-info-mark"));
    }

    #[test]
    fn invalid_rust_log_drops_the_bad_directive_and_keeps_the_rest() {
        let log = settings(LogLevel::Trace, &[]);
        let filter = build_filter(Some("!!!,warn"), &log);
        let out = captured(filter, emit_probe_events);
        assert!(
            out.contains("php-warn-mark"),
            "surviving directive lost:\n{out}"
        );
        assert!(
            !out.contains("rapira-info-mark"),
            "dropped directive must not widen the filter"
        );
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
    fn json_layer_emits_one_record_with_nested_fields() {
        let sink = Sink::default();
        let sub = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(make_layer(LogFormat::Json, false, sink.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::info_span!("req", rid = 1).in_scope(|| {
                tracing::info!(target: "rapira", answer = 42, "boot-mark");
            });
        });
        let out = sink.text();
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 1, "one JSON record: {out:?}");
        let v: serde_json::Value = serde_json::from_str(lines[0]).expect("json record");
        assert_eq!(v["fields"]["message"], "boot-mark");
        assert_eq!(v["fields"]["answer"], 42);
        assert_eq!(v["target"], "rapira");
        assert_eq!(v["level"], "INFO");
        // ChronoUtc %.3f: RFC 3339 UTC with exactly milliseconds.
        let ts = v["timestamp"].as_str().expect("timestamp");
        assert_eq!(ts.len(), "2026-01-01T00:00:00.000Z".len(), "{ts}");
        assert_eq!(&ts[19..20], ".");
        assert!(ts.ends_with('Z'));
        assert!(v.get("span").is_none() && v.get("spans").is_none());
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

    #[test]
    fn plain_layer_colors_only_when_asked() {
        for (ansi, want_escape) in [(false, false), (true, true)] {
            let sink = Sink::default();
            let sub = tracing_subscriber::registry()
                .with(EnvFilter::new("info"))
                .with(make_layer(LogFormat::Plain, ansi, sink.clone()));
            tracing::subscriber::with_default(sub, || {
                tracing::info!(target: "rapira", "color-mark");
            });
            let out = sink.text();
            assert!(out.contains("color-mark"));
            assert_eq!(out.contains('\u{1b}'), want_escape, "ansi={ansi}:\n{out}");
        }
    }
}
