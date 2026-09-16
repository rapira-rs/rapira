use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use opentelemetry::trace::{SpanId, TraceId, TracerProvider};
use opentelemetry::{Context, InstrumentationScope, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_proto::tonic::collector::{
    logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest,
};
use opentelemetry_proto::transform::{
    common::tonic::{Attributes, ResourceAttributesWithSchema},
    logs::tonic::group_logs_by_resource_and_scope,
    trace::tonic::group_spans_by_resource_and_scope,
};
use opentelemetry_sdk::logs::{LogBatch, LogProcessor, SdkLogRecord, SdkLoggerProvider};
use opentelemetry_sdk::trace::{
    IdGenerator, Sampler, SdkTracerProvider, Span, SpanData, SpanProcessor,
};
use opentelemetry_sdk::{Resource, error::OTelSdkResult};
use prost::Message;
use rand::Rng;
use rapira_config::OtelSettings;
use tracing::Subscriber;
use tracing_subscriber::{Layer, filter::filter_fn, registry::LookupSpan};

use crate::ipc::{Sender, Signal};

static ENABLED: AtomicBool = AtomicBool::new(false);
static POOL: Mutex<Option<&'static str>> = Mutex::new(None);
static INSTANCE_ID: LazyLock<Mutex<String>> =
    LazyLock::new(|| Mutex::new(format!("{:032x}", rand::rng().random::<u128>())));
static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn layer<S>(settings: &OtelSettings, sender: Sender) -> impl Layer<S> + Send + Sync
where
    S: Subscriber + Send + Sync + for<'a> LookupSpan<'a>,
{
    let mut layers: Vec<Box<dyn Layer<S> + Send + Sync>> = Vec::new();
    if !settings.enabled {
        return layers;
    }
    ENABLED.store(true, Relaxed);
    LazyLock::force(&INSTANCE_ID);
    let resource = Resource::builder_empty()
        .with_service_name(settings.service_name.clone())
        .with_detector(Box::new(
            opentelemetry_sdk::resource::TelemetryResourceDetector,
        ))
        .build();
    let mut provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            settings.sample_ratio,
        ))))
        .with_id_generator(ForkIdGenerator);
    if settings.traces {
        provider = provider.with_span_processor(Processor {
            sender: sender.clone(),
            resource: resource.clone(),
        });
    }
    let tracer = provider.build().tracer_with_scope(scope());
    layers.push(
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_context_activation(true)
            .with_threads(false)
            .with_filter(filter_fn(allowed))
            .boxed(),
    );
    if settings.logs {
        let provider = SdkLoggerProvider::builder()
            .with_resource(resource.clone())
            .with_log_processor(Processor {
                sender: sender.clone(),
                resource: resource.clone(),
            })
            .build();
        layers.push(
            OpenTelemetryTracingBridge::new(&provider)
                .with_filter(filter_fn(allowed))
                .boxed(),
        );
    }
    if settings.metrics {
        crate::metrics::init(resource, sender);
    }
    layers
}

fn allowed(metadata: &tracing::Metadata<'_>) -> bool {
    let target = metadata.target();
    !target.starts_with("opentelemetry")
        && !target.starts_with("tracing_opentelemetry")
        && !target.starts_with("otel::exporter")
        && !target.starts_with("otel::ipc")
        && target != "otel"
}

pub fn after_fork(pool: &'static str) {
    if !ENABLED.load(Relaxed) {
        return;
    }
    rand::rng()
        .reseed()
        .expect("reseed telemetry IDs after fork");
    *INSTANCE_ID.lock().unwrap() = format!("{:032x}", rand::rng().random::<u128>());
    *POOL.lock().unwrap() = Some(pool);
    DROPPED.store(0, Relaxed);
    crate::metrics::after_fork();
}

pub(crate) fn scope() -> InstrumentationScope {
    InstrumentationScope::builder("rapira")
        .with_version(env!("CARGO_PKG_VERSION"))
        .build()
}

pub(crate) fn process_resource(resource: &Resource) -> ResourceAttributesWithSchema {
    let mut resource = ResourceAttributesWithSchema::from(resource);
    let pool = *POOL.lock().unwrap();
    resource.attributes.0.extend(
        Attributes::from([
            KeyValue::new("service.instance.id", INSTANCE_ID.lock().unwrap().clone()),
            KeyValue::new("process.pid", i64::from(std::process::id())),
            KeyValue::new(
                "rapira.role",
                if pool.is_some() { "worker" } else { "master" },
            ),
        ])
        .0,
    );
    if let Some(pool) = pool {
        resource
            .attributes
            .0
            .extend(Attributes::from([KeyValue::new("rapira.pool", pool)]).0);
    }
    resource
}

pub(crate) fn send(sender: &Sender, signal: Signal, message: impl Message) {
    if !sender.send(signal, &message.encode_to_vec()) {
        DROPPED.fetch_add(1, Relaxed);
    }
}

pub(crate) fn dropped_records() -> u64 {
    DROPPED.load(Relaxed)
}

#[derive(Debug)]
struct ForkIdGenerator;

impl IdGenerator for ForkIdGenerator {
    fn new_trace_id(&self) -> TraceId {
        TraceId::from(rand::rng().random_range(1..=u128::MAX))
    }

    fn new_span_id(&self) -> SpanId {
        SpanId::from(rand::rng().random_range(1..=u64::MAX))
    }
}

struct Processor {
    sender: Sender,
    resource: Resource,
}

impl fmt::Debug for Processor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Processor").finish_non_exhaustive()
    }
}

impl SpanProcessor for Processor {
    fn on_start(&self, _span: &mut Span, _cx: &Context) {}

    fn on_end(&self, span: SpanData) {
        if span.span_context.is_sampled() {
            let resource = process_resource(&self.resource);
            send(
                &self.sender,
                Signal::Traces,
                ExportTraceServiceRequest {
                    resource_spans: group_spans_by_resource_and_scope(vec![span], &resource),
                },
            );
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

impl LogProcessor for Processor {
    fn emit(&self, record: &mut SdkLogRecord, scope: &InstrumentationScope) {
        let resource = process_resource(&self.resource);
        send(
            &self.sender,
            Signal::Logs,
            ExportLogsServiceRequest {
                resource_logs: group_logs_by_resource_and_scope(
                    &LogBatch::new(&[(record, scope)]),
                    &resource,
                ),
            },
        );
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderMap;
    use opentelemetry_proto::tonic::collector::{
        logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest,
    };
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use prost::Message;
    use tracing_subscriber::{filter::LevelFilter, prelude::*};

    use crate::tests::{Wire, attribute, settings};

    #[test]
    fn forked_records_use_worker_identity_and_fresh_ids() {
        const CHILD: &str = "RAPIRA_OTEL_FORK_TEST";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sdk::tests::forked_records_use_worker_identity_and_fresh_ids",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let wire = Wire::new();
        let subscriber =
            tracing_subscriber::registry().with(super::layer(&settings(), wire.sender.clone()));
        let child_pid = tracing::subscriber::with_default(subscriber, || {
            drop(crate::server_span(&HeaderMap::new(), "HEAD"));
            // SAFETY: this child test runs one test thread with a threadless SDK and no active spans.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                super::after_fork("http-test");
            }
            {
                let span = crate::server_span(&HeaderMap::new(), "GET");
                let _entered = span.enter();
                tracing::info!("process log");
            }
            if pid == 0 {
                // SAFETY: the child has sent all records and has no pending SDK work.
                unsafe { libc::_exit(0) }
            }
            let mut status = 0;
            // SAFETY: pid is the child just created and status is writable.
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert_eq!(status, 0);
            pid
        });
        let records = wire.records();
        let traces: Vec<_> = records
            .iter()
            .filter(|(signal, _)| *signal == 1)
            .flat_map(|(_, bytes)| {
                ExportTraceServiceRequest::decode(bytes.as_slice())
                    .unwrap()
                    .resource_spans
            })
            .collect();
        assert_eq!(traces.len(), 3);
        let worker = traces
            .iter()
            .find(|resource| {
                attribute(
                    &resource.resource.as_ref().unwrap().attributes,
                    "process.pid",
                ) == Some(&Value::IntValue(child_pid as i64))
            })
            .unwrap();
        let worker_attributes = &worker.resource.as_ref().unwrap().attributes;
        assert_eq!(
            attribute(worker_attributes, "rapira.role"),
            Some(&Value::StringValue("worker".into()))
        );
        assert_eq!(
            attribute(worker_attributes, "rapira.pool"),
            Some(&Value::StringValue("http-test".into()))
        );
        let worker_span = &worker.scope_spans[0].spans[0];
        let parent_span = traces
            .iter()
            .find(|resource| {
                attribute(
                    &resource.resource.as_ref().unwrap().attributes,
                    "process.pid",
                ) == Some(&Value::IntValue(std::process::id() as i64))
                    && resource.scope_spans[0].spans[0].name == "GET"
            })
            .map(|resource| &resource.scope_spans[0].spans[0])
            .unwrap();
        assert_ne!(worker_span.trace_id, parent_span.trace_id);
        assert_ne!(worker_span.span_id, parent_span.span_id);
        let logs: Vec<_> = records
            .iter()
            .filter(|(signal, _)| *signal == 2)
            .flat_map(|(_, bytes)| {
                ExportLogsServiceRequest::decode(bytes.as_slice())
                    .unwrap()
                    .resource_logs
            })
            .collect();
        assert_eq!(logs.len(), 2);
        let worker_log = logs
            .iter()
            .find(|resource| {
                attribute(
                    &resource.resource.as_ref().unwrap().attributes,
                    "process.pid",
                ) == Some(&Value::IntValue(child_pid as i64))
            })
            .unwrap();
        assert_eq!(
            attribute(
                &worker_log.resource.as_ref().unwrap().attributes,
                "rapira.pool"
            ),
            Some(&Value::StringValue("http-test".into()))
        );
        assert_eq!(
            worker_log.scope_logs[0].log_records[0].trace_id,
            worker_span.trace_id
        );
        let parent_resources: Vec<_> = traces
            .iter()
            .filter(|resource| {
                attribute(
                    &resource.resource.as_ref().unwrap().attributes,
                    "process.pid",
                ) == Some(&Value::IntValue(std::process::id() as i64))
            })
            .collect();
        let instance_id =
            |attributes: &[opentelemetry_proto::tonic::common::v1::KeyValue]| match attribute(
                attributes,
                "service.instance.id",
            ) {
                Some(Value::StringValue(value)) if !value.is_empty() => value.clone(),
                _ => panic!("each process must have a service.instance.id"),
            };
        let parent_id = instance_id(&parent_resources[0].resource.as_ref().unwrap().attributes);
        assert_eq!(parent_resources.len(), 2);
        assert_eq!(
            parent_id,
            instance_id(&parent_resources[1].resource.as_ref().unwrap().attributes)
        );
        let worker_id = instance_id(worker_attributes);
        assert_ne!(parent_id, worker_id);
        assert_eq!(
            worker_id,
            instance_id(&worker_log.resource.as_ref().unwrap().attributes)
        );
        struct Case {
            name: &'static str,
            reinitialize: bool,
        }
        let cases = [
            Case {
                name: "first process initialization with unchanged PID",
                reinitialize: true,
            },
            Case {
                name: "stable records in one process",
                reinitialize: false,
            },
            Case {
                name: "replacement process initialization with unchanged PID",
                reinitialize: true,
            },
        ];
        let resource = opentelemetry_sdk::Resource::builder_empty().build();
        let mut previous = parent_id;
        for case in cases {
            if case.reinitialize {
                super::after_fork("same-pid");
            }
            let attributes = super::process_resource(&resource).attributes.0;
            assert_eq!(
                attribute(&attributes, "process.pid"),
                Some(&Value::IntValue(std::process::id() as i64))
            );
            let current = instance_id(&attributes);
            assert_eq!(current != previous, case.reinitialize, "{}", case.name);
            previous = current;
        }
    }

    #[test]
    fn logs_correlate_with_operation_spans_and_exclude_sdk_diagnostics() {
        struct Case {
            name: &'static str,
            traces: bool,
            logs: bool,
        }
        let cases = [
            Case {
                name: "traces and logs",
                traces: true,
                logs: true,
            },
            Case {
                name: "logs retain context without trace export",
                traces: false,
                logs: true,
            },
            Case {
                name: "trace export without logs",
                traces: true,
                logs: false,
            },
        ];
        for case in cases {
            let wire = Wire::new();
            let mut config = settings();
            config.traces = case.traces;
            config.logs = case.logs;
            let stderr = tracing_subscriber::fmt::layer()
                .with_writer(std::io::sink)
                .with_filter(LevelFilter::ERROR);
            let subscriber = tracing_subscriber::registry()
                .with(stderr)
                .with(super::layer(&config, wire.sender.clone()));
            let mut headers = HeaderMap::new();
            headers.insert(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                    .parse()
                    .unwrap(),
            );
            let (server, operation) = tracing::subscriber::with_default(subscriber, || {
                let server = crate::server_span(&headers, "POST");
                let server_carrier = crate::trace_context(&server);
                let _server = server.enter();
                let operation = tracing::trace_span!("php.execute");
                let operation_carrier = crate::trace_context(&operation);
                let _operation = operation.enter();
                tracing::info!(answer = 42, "request log");
                tracing::error!(target: "opentelemetry_sdk::trace", "internal diagnostic");
                tracing::error!(target: "otel::exporter", "export failed");
                tracing::error!(target: "otel", "exporter exited");
                (server_carrier, operation_carrier)
            });
            let server_id = server
                .iter()
                .find(|(key, _)| key == "traceparent")
                .unwrap()
                .1
                .split('-')
                .nth(2)
                .unwrap();
            let operation_id = operation
                .iter()
                .find(|(key, _)| key == "traceparent")
                .unwrap()
                .1
                .split('-')
                .nth(2)
                .unwrap();
            let records = wire.records();
            let traces: Vec<_> = records
                .iter()
                .filter(|(signal, _)| *signal == 1)
                .flat_map(|(_, bytes)| {
                    ExportTraceServiceRequest::decode(bytes.as_slice())
                        .unwrap()
                        .resource_spans
                })
                .collect();
            let logs: Vec<_> = records
                .iter()
                .filter(|(signal, _)| *signal == 2)
                .flat_map(|(_, bytes)| {
                    ExportLogsServiceRequest::decode(bytes.as_slice())
                        .unwrap()
                        .resource_logs
                })
                .collect();
            assert_eq!(
                traces.len(),
                if case.traces { 2 } else { 0 },
                "{}",
                case.name
            );
            assert_eq!(logs.len(), usize::from(case.logs), "{}", case.name);
            if let Some(resource) = traces.first() {
                let attributes = &resource.resource.as_ref().unwrap().attributes;
                assert_eq!(
                    attribute(attributes, "service.name"),
                    Some(&Value::StringValue("sdk-test".into()))
                );
                assert_eq!(
                    attribute(attributes, "process.pid"),
                    Some(&Value::IntValue(std::process::id() as i64))
                );
                assert_eq!(
                    attribute(attributes, "rapira.role"),
                    Some(&Value::StringValue("master".into()))
                );
                let scope = &resource.scope_spans[0];
                assert_eq!(scope.scope.as_ref().unwrap().name, "rapira");
                let span = &scope.spans[0];
                assert_eq!(span.name, "php.execute");
                assert_eq!(
                    span.parent_span_id,
                    u64::from_str_radix(server_id, 16).unwrap().to_be_bytes()
                );
            }
            if let Some(resource) = logs.first() {
                assert_eq!(
                    attribute(
                        &resource.resource.as_ref().unwrap().attributes,
                        "process.pid"
                    ),
                    Some(&Value::IntValue(std::process::id() as i64))
                );
                let log = &resource.scope_logs[0].log_records[0];
                assert_eq!(
                    log.trace_id,
                    [
                        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d,
                        0x0e, 0x0e, 0x47, 0x36
                    ]
                );
                assert_eq!(
                    log.span_id,
                    u64::from_str_radix(operation_id, 16).unwrap().to_be_bytes()
                );
                assert_eq!(
                    log.body.as_ref().unwrap().value,
                    Some(Value::StringValue("request log".into()))
                );
                assert_eq!(
                    attribute(&log.attributes, "answer"),
                    Some(&Value::IntValue(42))
                );
            }
        }
    }
}
