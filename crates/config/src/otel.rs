use anyhow::{Context, bail};
use http::{header::HeaderName, header::HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{MAX_TIMEOUT_SECS, capped_timeout};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtelSettings {
    pub enabled: bool,
    pub endpoint: String,
    pub service_name: String,
    pub sample_ratio: f64,
    pub traces: bool,
    pub logs: bool,
    pub metrics: bool,
    pub batch_size: usize,
    pub queue_size: usize,
    pub flush_interval_ms: u64,
    pub export_timeout_secs: u64,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OtelSection {
    enabled: Option<bool>,
    endpoint: Option<String>,
    service_name: Option<String>,
    sample_ratio: Option<f64>,
    traces: Option<bool>,
    logs: Option<bool>,
    metrics: Option<bool>,
    batch_size: Option<usize>,
    queue_size: Option<usize>,
    flush_interval_ms: Option<u64>,
    export_timeout_secs: Option<u64>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

pub(crate) fn resolve_otel(section: OtelSection) -> anyhow::Result<OtelSettings> {
    let endpoint = section
        .endpoint
        .unwrap_or_else(|| "http://localhost:4318".to_owned());
    let url = url::Url::parse(&endpoint).context("invalid otel.endpoint")?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        bail!("otel.endpoint must be an HTTP or HTTPS URL with a host");
    }

    let service_name = section.service_name.unwrap_or_else(|| "rapira".to_owned());
    if service_name.is_empty() {
        bail!("otel.service_name must not be empty");
    }

    let sample_ratio = section.sample_ratio.unwrap_or(1.0);
    if !(0.0..=1.0).contains(&sample_ratio) {
        bail!("otel.sample_ratio must be finite and between 0.0 and 1.0");
    }

    let batch_size = section.batch_size.unwrap_or(512);
    if batch_size == 0 {
        bail!("otel.batch_size must be at least 1");
    }
    let queue_size = section.queue_size.unwrap_or(2048);
    if queue_size == 0 {
        bail!("otel.queue_size must be at least 1");
    }
    if batch_size > queue_size {
        bail!("otel.batch_size ({batch_size}) must be at most otel.queue_size ({queue_size})");
    }

    let flush_interval_ms = section.flush_interval_ms.unwrap_or(1000);
    if flush_interval_ms == 0 {
        bail!("otel.flush_interval_ms must be at least 1");
    }
    const MAX_FLUSH_INTERVAL_MS: u64 = MAX_TIMEOUT_SECS * 1000;
    if flush_interval_ms > MAX_FLUSH_INTERVAL_MS {
        bail!(
            "otel.flush_interval_ms {flush_interval_ms} is too large (max {MAX_FLUSH_INTERVAL_MS})"
        );
    }

    let export_timeout_secs = section.export_timeout_secs.unwrap_or(5);
    if export_timeout_secs == 0 {
        bail!("otel.export_timeout_secs must be at least 1");
    }
    capped_timeout("otel", "export_timeout_secs", export_timeout_secs)?;

    for (name, value) in &section.headers {
        HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid otel.headers name `{}`", name.escape_default()))?;
        HeaderValue::from_str(value).with_context(|| {
            format!("invalid otel.headers value for `{}`", name.escape_default())
        })?;
    }

    Ok(OtelSettings {
        enabled: section.enabled.unwrap_or(false),
        endpoint,
        service_name,
        sample_ratio,
        traces: section.traces.unwrap_or(true),
        logs: section.logs.unwrap_or(true),
        metrics: section.metrics.unwrap_or(true),
        batch_size,
        queue_size,
        flush_interval_ms,
        export_timeout_secs,
        headers: section.headers,
    })
}

#[cfg(test)]
mod tests {
    use super::OtelSettings;
    use crate::{load_str, merge};
    use std::collections::BTreeMap;
    use std::path::Path;

    fn config(section: &str) -> String {
        format!("[http.pool]\nentrypoint = \"app.php\"\nmode = \"worker\"\n{section}\n")
    }

    #[test]
    fn otel_valid_settings_resolve() {
        struct Case {
            name: &'static str,
            section: &'static str,
            enabled: bool,
            endpoint: &'static str,
            service_name: &'static str,
            sample_ratio: f64,
            signals: (bool, bool, bool),
            batch_size: usize,
            queue_size: usize,
            flush_interval_ms: u64,
            export_timeout_secs: u64,
            headers: &'static [(&'static str, &'static str)],
        }

        let cases = [
            Case {
                name: "omitted table with a worker pool",
                section: "",
                enabled: false,
                endpoint: "http://localhost:4318",
                service_name: "rapira",
                sample_ratio: 1.0,
                signals: (true, true, true),
                batch_size: 512,
                queue_size: 2048,
                flush_interval_ms: 1000,
                export_timeout_secs: 5,
                headers: &[],
            },
            Case {
                name: "enabled with default export settings",
                section: "[otel]\nenabled = true",
                enabled: true,
                endpoint: "http://localhost:4318",
                service_name: "rapira",
                sample_ratio: 1.0,
                signals: (true, true, true),
                batch_size: 512,
                queue_size: 2048,
                flush_interval_ms: 1000,
                export_timeout_secs: 5,
                headers: &[],
            },
            Case {
                name: "disabled with user export settings",
                section: r#"
                    [otel]
                    enabled = false
                    endpoint = "https://collector.example:8443/tenant"
                    service_name = "api-worker"
                    sample_ratio = 0.25
                    traces = false
                    logs = true
                    metrics = false
                    batch_size = 16
                    queue_size = 64
                    flush_interval_ms = 2500
                    export_timeout_secs = 10
                    [otel.headers]
                    Authorization = "Bearer collector-token"
                    X-Tenant-Id = "tenant-a"
                "#,
                enabled: false,
                endpoint: "https://collector.example:8443/tenant",
                service_name: "api-worker",
                sample_ratio: 0.25,
                signals: (false, true, false),
                batch_size: 16,
                queue_size: 64,
                flush_interval_ms: 2500,
                export_timeout_secs: 10,
                headers: &[
                    ("Authorization", "Bearer collector-token"),
                    ("X-Tenant-Id", "tenant-a"),
                ],
            },
            Case {
                name: "minimum ratio and time values",
                section: r#"
                    [otel]
                    enabled = true
                    sample_ratio = 0.0
                    batch_size = 1
                    queue_size = 1
                    flush_interval_ms = 1
                    export_timeout_secs = 1
                "#,
                enabled: true,
                endpoint: "http://localhost:4318",
                service_name: "rapira",
                sample_ratio: 0.0,
                signals: (true, true, true),
                batch_size: 1,
                queue_size: 1,
                flush_interval_ms: 1,
                export_timeout_secs: 1,
                headers: &[],
            },
            Case {
                name: "maximum ratio and time values",
                section: r#"
                    [otel]
                    enabled = true
                    sample_ratio = 1.0
                    batch_size = 4096
                    queue_size = 8192
                    flush_interval_ms = 86400000
                    export_timeout_secs = 86400
                "#,
                enabled: true,
                endpoint: "http://localhost:4318",
                service_name: "rapira",
                sample_ratio: 1.0,
                signals: (true, true, true),
                batch_size: 4096,
                queue_size: 8192,
                flush_interval_ms: 86400000,
                export_timeout_secs: 86400,
                headers: &[],
            },
            Case {
                name: "traces only with a user sample ratio",
                section: r#"
                    [otel]
                    enabled = true
                    endpoint = "http://127.0.0.1:4318/collector/"
                    sample_ratio = 0.125
                    traces = true
                    logs = false
                    metrics = false
                "#,
                enabled: true,
                endpoint: "http://127.0.0.1:4318/collector/",
                service_name: "rapira",
                sample_ratio: 0.125,
                signals: (true, false, false),
                batch_size: 512,
                queue_size: 2048,
                flush_interval_ms: 1000,
                export_timeout_secs: 5,
                headers: &[],
            },
            Case {
                name: "metrics only with IPv6 endpoint",
                section: r#"
                    [otel]
                    enabled = true
                    endpoint = "http://[::1]:4318"
                    traces = false
                    logs = false
                    metrics = true
                    headers = { "x-empty" = "" }
                "#,
                enabled: true,
                endpoint: "http://[::1]:4318",
                service_name: "rapira",
                sample_ratio: 1.0,
                signals: (false, false, true),
                batch_size: 512,
                queue_size: 2048,
                flush_interval_ms: 1000,
                export_timeout_secs: 5,
                headers: &[("x-empty", "")],
            },
            Case {
                name: "all signals disabled",
                section: r#"
                    [otel]
                    enabled = true
                    traces = false
                    logs = false
                    metrics = false
                "#,
                enabled: true,
                endpoint: "http://localhost:4318",
                service_name: "rapira",
                sample_ratio: 1.0,
                signals: (false, false, false),
                batch_size: 512,
                queue_size: 2048,
                flush_interval_ms: 1000,
                export_timeout_secs: 5,
                headers: &[],
            },
        ];

        fn assert_settings(settings: &OtelSettings, case: &Case) {
            assert_eq!(settings.enabled, case.enabled, "{}", case.name);
            assert_eq!(settings.endpoint, case.endpoint, "{}", case.name);
            assert_eq!(settings.service_name, case.service_name, "{}", case.name);
            assert_eq!(settings.sample_ratio, case.sample_ratio, "{}", case.name);
            assert_eq!(
                (settings.traces, settings.logs, settings.metrics),
                case.signals,
                "{}",
                case.name
            );
            assert_eq!(settings.batch_size, case.batch_size, "{}", case.name);
            assert_eq!(settings.queue_size, case.queue_size, "{}", case.name);
            assert_eq!(
                settings.flush_interval_ms, case.flush_interval_ms,
                "{}",
                case.name
            );
            assert_eq!(
                settings.export_timeout_secs, case.export_timeout_secs,
                "{}",
                case.name
            );
            let headers: BTreeMap<String, String> = case
                .headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect();
            assert_eq!(settings.headers, headers, "{}", case.name);
        }

        for case in cases {
            let settings = load_str(&config(case.section))
                .and_then(|file| merge(file, Some(Path::new("/srv/app"))))
                .unwrap_or_else(|err| panic!("{}: {err}", case.name));
            assert_settings(&settings.otel, &case);

            let encoded = toml::to_string(&settings.otel.clone()).unwrap();
            let decoded: OtelSettings = toml::from_str(&encoded).unwrap();
            assert_settings(&decoded, &case);
        }
    }

    #[test]
    fn otel_invalid_buffer_sizes_are_rejected() {
        struct Case {
            name: &'static str,
            values: &'static str,
            key: &'static str,
        }

        let cases = [
            Case {
                name: "zero batch size",
                values: "batch_size = 0",
                key: "otel.batch_size",
            },
            Case {
                name: "zero queue size",
                values: "queue_size = 0",
                key: "otel.queue_size",
            },
            Case {
                name: "batch larger than queue",
                values: "batch_size = 2\nqueue_size = 1",
                key: "otel.batch_size",
            },
            Case {
                name: "batch larger than default queue",
                values: "batch_size = 2049",
                key: "otel.batch_size",
            },
            Case {
                name: "queue smaller than default batch",
                values: "queue_size = 511",
                key: "otel.queue_size",
            },
        ];

        let mut accepted = Vec::new();
        for case in cases {
            let file = load_str(&config(&format!("[otel]\n{}", case.values)))
                .unwrap_or_else(|err| panic!("{}: {err}", case.name));
            match merge(file, Some(Path::new("/srv/app"))) {
                Ok(_) => accepted.push(case.name),
                Err(err) => assert!(err.to_string().contains(case.key), "{}: {err}", case.name),
            }
        }
        assert!(accepted.is_empty(), "invalid sizes accepted: {accepted:?}");
    }

    #[test]
    fn otel_endpoint_authorities_are_validated() {
        struct Case {
            name: &'static str,
            endpoint: &'static str,
            valid: bool,
        }

        let cases = [
            Case {
                name: "invalid port 70000",
                endpoint: "http://localhost:70000",
                valid: false,
            },
            Case {
                name: "invalid port abc",
                endpoint: "http://localhost:abc",
                valid: false,
            },
            Case {
                name: "invalid IPv6 literal",
                endpoint: "http://[invalid]:4318",
                valid: false,
            },
            Case {
                name: "maximum port number",
                endpoint: "https://collector.example:65535/tenant",
                valid: true,
            },
            Case {
                name: "IPv6 loopback with port",
                endpoint: "http://[::1]:4318",
                valid: true,
            },
            Case {
                name: "IPv6 address with default port",
                endpoint: "https://[2001:db8::1]/tenant",
                valid: true,
            },
        ];

        let mut accepted = Vec::new();
        for case in cases {
            let section = format!("[otel]\nendpoint = {:?}", case.endpoint);
            let file =
                load_str(&config(&section)).unwrap_or_else(|err| panic!("{}: {err}", case.name));
            let result = merge(file, Some(Path::new("/srv/app")));
            if case.valid {
                let settings = result.unwrap_or_else(|err| panic!("{}: {err}", case.name));
                assert_eq!(settings.otel.endpoint, case.endpoint, "{}", case.name);
            } else {
                match result {
                    Ok(_) => accepted.push(case.name),
                    Err(err) => assert!(
                        err.to_string().contains("otel.endpoint"),
                        "{}: {err}",
                        case.name
                    ),
                }
            }
        }
        assert!(
            accepted.is_empty(),
            "invalid endpoints accepted: {accepted:?}"
        );
    }

    #[test]
    fn otel_invalid_values_name_the_key() {
        struct Case {
            name: &'static str,
            values: &'static str,
            key: &'static str,
        }

        let cases = [
            Case {
                name: "unsupported endpoint scheme",
                values: "endpoint = \"grpc://localhost:4317\"",
                key: "otel.endpoint",
            },
            Case {
                name: "endpoint without scheme",
                values: "endpoint = \"localhost:4318\"",
                key: "otel.endpoint",
            },
            Case {
                name: "endpoint without host",
                values: "endpoint = \"http://\"",
                key: "otel.endpoint",
            },
            Case {
                name: "endpoint with a port but no host",
                values: "endpoint = \"http://:4318\"",
                key: "otel.endpoint",
            },
            Case {
                name: "endpoint with invalid host",
                values: "endpoint = \"https://invalid host:4318\"",
                key: "otel.endpoint",
            },
            Case {
                name: "empty service name",
                values: "service_name = \"\"",
                key: "otel.service_name",
            },
            Case {
                name: "sample ratio below zero",
                values: "sample_ratio = -0.001",
                key: "otel.sample_ratio",
            },
            Case {
                name: "sample ratio above one",
                values: "sample_ratio = 1.001",
                key: "otel.sample_ratio",
            },
            Case {
                name: "sample ratio is NaN",
                values: "sample_ratio = nan",
                key: "otel.sample_ratio",
            },
            Case {
                name: "sample ratio is positive infinity",
                values: "sample_ratio = inf",
                key: "otel.sample_ratio",
            },
            Case {
                name: "sample ratio is negative infinity",
                values: "sample_ratio = -inf",
                key: "otel.sample_ratio",
            },
            Case {
                name: "zero flush interval",
                values: "flush_interval_ms = 0",
                key: "otel.flush_interval_ms",
            },
            Case {
                name: "flush interval above one day",
                values: "flush_interval_ms = 86400001",
                key: "otel.flush_interval_ms",
            },
            Case {
                name: "zero export timeout",
                values: "export_timeout_secs = 0",
                key: "otel.export_timeout_secs",
            },
            Case {
                name: "export timeout above one day",
                values: "export_timeout_secs = 86401",
                key: "otel.export_timeout_secs",
            },
            Case {
                name: "empty header name",
                values: "headers = { \"\" = \"value\" }",
                key: "otel.headers",
            },
            Case {
                name: "header name with a space",
                values: "headers = { \"x tenant\" = \"tenant-a\" }",
                key: "otel.headers",
            },
            Case {
                name: "header value with a line break",
                values: r#"headers = { Authorization = "token\r\nX-Injected: true" }"#,
                key: "otel.headers",
            },
            Case {
                name: "invalid ratio while disabled",
                values: "enabled = false\nsample_ratio = 2.0",
                key: "otel.sample_ratio",
            },
        ];

        for case in cases {
            let file = load_str(&config(&format!("[otel]\n{}", case.values)))
                .unwrap_or_else(|err| panic!("{}: {err}", case.name));
            let err = merge(file, Some(Path::new("/srv/app")))
                .expect_err(case.name)
                .to_string();
            assert!(err.contains(case.key), "{}: {err}", case.name);
        }
    }

    #[test]
    fn otel_rejects_unknown_keys_and_wrong_types() {
        struct Case {
            name: &'static str,
            section: &'static str,
            error: &'static str,
        }

        let cases = [
            Case {
                name: "unknown signal key",
                section: "[otel]\ntrace = true",
                error: "unknown field `trace`",
            },
            Case {
                name: "unknown nested table",
                section: "[otel.exporter]\nendpoint = \"http://localhost:4318\"",
                error: "unknown field `exporter`",
            },
            Case {
                name: "telemetry table is unsupported",
                section: "[telemetry]\nenabled = true",
                error: "unknown field `telemetry`",
            },
            Case {
                name: "enabled must be a boolean",
                section: "[otel]\nenabled = \"true\"",
                error: "invalid type",
            },
            Case {
                name: "sample ratio must be numeric",
                section: "[otel]\nsample_ratio = \"0.5\"",
                error: "invalid type",
            },
            Case {
                name: "batch size must be unsigned",
                section: "[otel]\nbatch_size = -1",
                error: "invalid value",
            },
            Case {
                name: "queue size must be unsigned",
                section: "[otel]\nqueue_size = -1",
                error: "invalid value",
            },
            Case {
                name: "flush interval must be an integer",
                section: "[otel]\nflush_interval_ms = 1.5",
                error: "invalid type",
            },
            Case {
                name: "export timeout must be unsigned",
                section: "[otel]\nexport_timeout_secs = -1",
                error: "invalid value",
            },
            Case {
                name: "header values must be strings",
                section: "[otel.headers]\nx-tenant-id = 42",
                error: "invalid type",
            },
        ];

        for case in cases {
            let err = load_str(&config(case.section))
                .expect_err(case.name)
                .to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }
}
