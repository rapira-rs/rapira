use http::HeaderMap;
use tracing::Span;

#[cfg(not(feature = "sdk"))]
pub fn server_span(_headers: &HeaderMap, _method: &str) -> Span {
    Span::none()
}

#[cfg(not(feature = "sdk"))]
pub fn trace_context(_span: &Span) -> Vec<(String, String)> {
    Vec::new()
}

#[cfg(feature = "sdk")]
pub(crate) fn bounded_method(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "CONNECT" => "CONNECT",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "PATCH" => "PATCH",
        _ => "_OTHER",
    }
}

#[cfg(feature = "sdk")]
pub fn server_span(headers: &HeaderMap, method: &str) -> Span {
    use opentelemetry::{Context, propagation::TextMapPropagator};
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let method = bounded_method(method);
    let span = tracing::info_span!(
        parent: None,
        "http.server",
        otel.name = method,
        otel.kind = "server",
        http.request.method = method,
        http.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    );
    let parent =
        TraceContextPropagator::new().extract_with_context(&Context::new(), &Headers::new(headers));
    if span.set_parent(parent).is_err() {
        return Span::none();
    }
    span
}

#[cfg(feature = "sdk")]
pub fn trace_context(span: &Span) -> Vec<(String, String)> {
    use opentelemetry::propagation::{Injector, TextMapPropagator};
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    #[derive(Default)]
    struct Carrier(Vec<(String, String)>);

    impl Injector for Carrier {
        fn set(&mut self, key: &str, value: String) {
            if !value.is_empty() {
                self.0.push((key.to_owned(), value));
            }
        }
    }

    let mut carrier = Carrier::default();
    TraceContextPropagator::new().inject_context(&span.context(), &mut carrier);
    carrier.0
}

#[cfg(feature = "sdk")]
struct Headers<'a> {
    parent: Option<&'a str>,
    state: String,
}

#[cfg(feature = "sdk")]
impl<'a> Headers<'a> {
    fn new(headers: &'a HeaderMap) -> Self {
        let mut parents = headers.get_all("traceparent").iter();
        let parent = parents
            .next()
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| {
                if value.starts_with("00-") && value.len() != 55 {
                    return false;
                }
                let mut fields = value.split('-');
                [2, 32, 16, 2].into_iter().all(|length| {
                    fields.next().is_some_and(|field| {
                        field.len() == length
                            && field
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    })
                }) && parents.next().is_none()
            });
        let state = parent
            .and_then(|_| normalized_tracestate(headers))
            .unwrap_or_default();
        Self { parent, state }
    }
}

#[cfg(feature = "sdk")]
fn normalized_tracestate(headers: &HeaderMap) -> Option<String> {
    // W3C list members have unique keys and optional whitespace: https://www.w3.org/TR/trace-context/#tracestate-header-field-values
    fn key_part(part: &str, max_length: usize, digit_start: bool) -> bool {
        part.as_bytes().first().is_some_and(|first| {
            part.len() <= max_length
                && (first.is_ascii_lowercase() || (digit_start && first.is_ascii_digit()))
                && part.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-' | b'*' | b'/')
                })
        })
    }

    let mut keys = Vec::new();
    let mut members = Vec::new();
    for header in headers.get_all("tracestate") {
        for member in header.to_str().ok()?.split(',') {
            let member = member.trim_matches([' ', '\t']);
            if member.is_empty() {
                continue;
            }
            let (key, value) = member.split_once('=')?;
            let valid_key = match key.split_once('@') {
                Some((tenant, system)) => {
                    key_part(tenant, 241, true) && key_part(system, 14, false)
                }
                None => key_part(key, 256, false),
            };
            if !valid_key
                || value.is_empty()
                || value.len() > 256
                || keys.len() == 32
                || keys.contains(&key)
                || !value
                    .bytes()
                    .all(|byte| (b' '..=b'~').contains(&byte) && byte != b'=')
            {
                return None;
            }
            keys.push(key);
            members.push(member);
        }
    }
    Some(members.join(","))
}

#[cfg(feature = "sdk")]
impl opentelemetry::propagation::Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        match key {
            "traceparent" => self.parent,
            "tracestate" => Some(&self.state),
            _ => None,
        }
    }

    fn keys(&self) -> Vec<&str> {
        vec!["traceparent", "tracestate"]
    }
}

#[cfg(all(test, feature = "sdk"))]
mod tests {
    use super::*;
    use opentelemetry::Context;
    use opentelemetry::propagation::TextMapPropagator;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use prost::Message;
    use tracing_subscriber::prelude::*;

    use crate::tests::{Wire, settings};

    const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT_ID: &str = "00f067aa0ba902b7";

    #[test]
    fn request_context_continues_remote_parent_and_isolates_invalid_input() {
        struct Case {
            name: &'static str,
            parent: Option<&'static str>,
            state: &'static [&'static str],
            ratio: f64,
            method: &'static str,
            expected_method: &'static str,
            remote: bool,
            sampled: bool,
            expected_state: Option<&'static str>,
        }
        let cases = [
            Case {
                name: "sampled W3C parent overrides zero ratio",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &["vendor=a"],
                ratio: 0.0,
                method: "GET",
                expected_method: "GET",
                remote: true,
                sampled: true,
                expected_state: Some("vendor=a"),
            },
            Case {
                name: "unsampled W3C parent overrides full ratio",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"),
                state: &[],
                ratio: 1.0,
                method: "POST",
                expected_method: "POST",
                remote: true,
                sampled: false,
                expected_state: None,
            },
            Case {
                name: "invalid tracestate preserves parent",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &["invalid"],
                ratio: 1.0,
                method: "PATCH",
                expected_method: "PATCH",
                remote: true,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "repeated tracestate fields retain order",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &["first=a", "second=b"],
                ratio: 1.0,
                method: "HEAD",
                expected_method: "HEAD",
                remote: true,
                sampled: true,
                expected_state: Some("first=a,second=b"),
            },
            Case {
                name: "tracestate list whitespace retains members",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &[" first=a , second=b ", "third=c"],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: true,
                sampled: true,
                expected_state: Some("first=a,second=b,third=c"),
            },
            Case {
                name: "duplicate tracestate keys preserve only the parent",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &["vendor=a", "vendor=b"],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: true,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "empty tracestate value preserves only the parent",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &["vendor="],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: true,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "invalid tracestate value preserves only the parent",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &["vendor==value"],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: true,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "valid multi-tenant tracestate retains its value",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &["12@vendor=value"],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: true,
                sampled: true,
                expected_state: Some("12@vendor=value"),
            },
            Case {
                name: "incomplete tenant key preserves only the parent",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
                state: &["tenant@=value"],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: true,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "missing parent ignores ambient context",
                parent: None,
                state: &[],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: false,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "malformed parent ignores ambient context",
                parent: Some("invalid"),
                state: &["vendor=a"],
                ratio: 1.0,
                method: "CUSTOM-UNBOUNDED",
                expected_method: "_OTHER",
                remote: false,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "zero trace ID starts an isolated root",
                parent: Some("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
                state: &[],
                ratio: 1.0,
                method: "PUT",
                expected_method: "PUT",
                remote: false,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "short trace ID starts an isolated root",
                parent: Some("00-4bf92f35-00f067aa0ba902b7-01"),
                state: &[],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: false,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "short flags start an isolated root",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1"),
                state: &[],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: false,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "version zero trailing delimiter starts an isolated root",
                parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-"),
                state: &[],
                ratio: 1.0,
                method: "GET",
                expected_method: "GET",
                remote: false,
                sampled: true,
                expected_state: None,
            },
            Case {
                name: "zero ratio root propagates without export",
                parent: None,
                state: &[],
                ratio: 0.0,
                method: "DELETE",
                expected_method: "DELETE",
                remote: false,
                sampled: false,
                expected_state: None,
            },
        ];
        for case in cases {
            let wire = Wire::new();
            let mut config = settings();
            config.sample_ratio = case.ratio;
            let subscriber =
                tracing_subscriber::registry().with(crate::layer(&config, wire.sender.clone()));
            let mut headers = HeaderMap::new();
            if let Some(parent) = case.parent {
                headers.insert("traceparent", parent.parse().unwrap());
            }
            for state in case.state {
                headers.append("tracestate", state.parse().unwrap());
            }
            let ambient = std::collections::HashMap::from([(
                "traceparent".into(),
                "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into(),
            )]);
            let ambient =
                TraceContextPropagator::new().extract_with_context(&Context::new(), &ambient);
            let _ambient_guard = ambient.attach();
            let carrier = tracing::subscriber::with_default(subscriber, || {
                let outer = tracing::info_span!("ambient_tracing");
                let _entered = outer.enter();
                let span = server_span(&headers, case.method);
                let carrier = trace_context(&span);
                assert!(!span.is_disabled(), "{}", case.name);
                carrier
            });
            let parent = carrier
                .iter()
                .find(|(key, _)| key == "traceparent")
                .unwrap_or_else(|| panic!("{}: missing traceparent", case.name));
            let parts: Vec<_> = parent.1.split('-').collect();
            assert_eq!(parts.len(), 4, "{}", case.name);
            assert_eq!(parts[0], "00", "{}", case.name);
            assert_eq!(
                parts[3],
                if case.sampled { "01" } else { "00" },
                "{}",
                case.name
            );
            assert_ne!(parts[2], PARENT_ID, "{}", case.name);
            assert_ne!(parts[2], "0000000000000000", "{}", case.name);
            if case.remote {
                assert_eq!(parts[1], TRACE_ID, "{}", case.name);
            } else {
                assert_ne!(parts[1], TRACE_ID, "{}", case.name);
                assert_ne!(
                    parts[1], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "{}",
                    case.name
                );
                assert_ne!(
                    parts[1], "0000000000000000000000004bf92f35",
                    "{}",
                    case.name
                );
            }
            assert_eq!(
                carrier
                    .iter()
                    .find(|(key, _)| key == "tracestate")
                    .map(|(_, value)| value.as_str()),
                case.expected_state,
                "{}",
                case.name
            );
            let spans: Vec<_> = wire
                .records()
                .into_iter()
                .filter(|(signal, _)| *signal == 1)
                .flat_map(|(_, bytes)| {
                    ExportTraceServiceRequest::decode(bytes.as_slice())
                        .unwrap()
                        .resource_spans
                })
                .flat_map(|resource| resource.scope_spans)
                .flat_map(|scope| scope.spans)
                .filter(|span| span.name != "ambient_tracing")
                .collect();
            assert_eq!(spans.len(), usize::from(case.sampled), "{}", case.name);
            if let Some(span) = spans.first() {
                assert_eq!(span.name, case.expected_method, "{}", case.name);
                assert_eq!(span.kind, 2, "{}", case.name);
                assert_eq!(
                    span.trace_id,
                    u128::from_str_radix(parts[1], 16).unwrap().to_be_bytes(),
                    "{}",
                    case.name
                );
                assert_eq!(
                    span.span_id,
                    u64::from_str_radix(parts[2], 16).unwrap().to_be_bytes(),
                    "{}",
                    case.name
                );
                if case.remote {
                    assert_eq!(
                        span.parent_span_id,
                        [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7],
                        "{}",
                        case.name
                    );
                } else {
                    assert!(span.parent_span_id.is_empty(), "{}", case.name);
                }
            }
        }
    }

    #[test]
    fn inactive_sdk_has_no_server_span_or_carrier() {
        struct Case {
            name: &'static str,
            configure_layer: bool,
        }
        let cases = [
            Case {
                name: "subscriber without SDK",
                configure_layer: false,
            },
            Case {
                name: "runtime disabled SDK",
                configure_layer: true,
            },
        ];
        for case in cases {
            let wire = Wire::new();
            let mut config = settings();
            config.enabled = false;
            let sdk = case
                .configure_layer
                .then(|| crate::layer(&config, wire.sender.clone()));
            tracing::subscriber::with_default(tracing_subscriber::registry().with(sdk), || {
                let span = server_span(&HeaderMap::new(), "GET");
                assert!(span.is_disabled(), "{}", case.name);
                assert!(trace_context(&span).is_empty(), "{}", case.name);
            });
            assert!(wire.records().is_empty(), "{}", case.name);
        }
    }
}
