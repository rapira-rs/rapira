use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use opentelemetry::{
    KeyValue,
    metrics::{Counter, Histogram, MeterProvider},
};
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_sdk::metrics::{
    InstrumentKind, ManualReader, Pipeline, SdkMeterProvider, Temporality, data::ResourceMetrics,
    reader::MetricReader,
};
use opentelemetry_sdk::{Resource, error::OTelSdkResult};

use crate::{
    context::bounded_method,
    ipc::{Sender, Signal},
    sdk,
};

static METRICS: Mutex<Option<Metrics>> = Mutex::new(None);
const SECOND_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

pub(crate) fn init(resource: Resource, sender: Sender) {
    *METRICS.lock().unwrap() = Some(Metrics::new(resource, sender));
}

pub(crate) fn after_fork() {
    let mut metrics = METRICS.lock().unwrap();
    if let Some(previous) = metrics.take() {
        *metrics = Some(Metrics::new(
            previous.resource.clone(),
            previous.sender.clone(),
        ));
    }
}

pub fn request_finished(method: &str, status: u16, elapsed: Duration) {
    if let Some(metrics) = METRICS.lock().unwrap().as_mut() {
        metrics.request_duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("http.request.method", bounded_method(method)),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ],
        );
        metrics.collect();
    }
}

pub fn record_duration(operation: &'static str, elapsed: Duration) {
    if let Some(metrics) = METRICS.lock().unwrap().as_mut() {
        metrics.operation_duration.record(
            elapsed.as_secs_f64(),
            &[KeyValue::new("rapira.operation", operation)],
        );
        metrics.collect();
    }
}

struct Metrics {
    _provider: SdkMeterProvider,
    reader: Reader,
    resource: Resource,
    sender: Sender,
    request_duration: Histogram<f64>,
    operation_duration: Histogram<f64>,
    dropped: Counter<u64>,
    last_dropped: u64,
}

impl Metrics {
    fn new(resource: Resource, sender: Sender) -> Self {
        let reader = Reader(Arc::new(
            ManualReader::builder()
                .with_temporality(Temporality::Cumulative)
                .build(),
        ));
        let provider = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_reader(reader.clone())
            .build();
        let meter = provider.meter_with_scope(sdk::scope());
        let request_duration = meter
            .f64_histogram("http.server.request.duration")
            .with_unit("s")
            .with_boundaries(SECOND_BUCKETS.to_vec())
            .build();
        let operation_duration = meter
            .f64_histogram("rapira.operation.duration")
            .with_unit("s")
            .with_boundaries(SECOND_BUCKETS.to_vec())
            .build();
        let dropped = meter
            .u64_counter("rapira.otel.dropped_records")
            .with_unit("{record}")
            .build();
        Self {
            _provider: provider,
            reader,
            resource,
            sender,
            request_duration,
            operation_duration,
            dropped,
            last_dropped: 0,
        }
    }

    fn collect(&mut self) {
        let dropped = sdk::dropped_records();
        self.dropped
            .add(dropped.saturating_sub(self.last_dropped), &[]);
        self.last_dropped = dropped;
        let mut snapshot = ResourceMetrics::default();
        if self.reader.collect(&mut snapshot).is_ok() {
            let mut request = ExportMetricsServiceRequest::from(&snapshot);
            for resource_metrics in &mut request.resource_metrics {
                if let Some(resource) = &mut resource_metrics.resource {
                    resource.attributes = sdk::process_resource(&self.resource).attributes.0;
                }
            }
            sdk::send(&self.sender, Signal::Metrics, request);
        }
    }
}

#[derive(Clone, Debug)]
struct Reader(Arc<ManualReader>);

impl MetricReader for Reader {
    fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
        self.0.register_pipeline(pipeline);
    }

    fn collect(&self, metrics: &mut ResourceMetrics) -> OTelSdkResult {
        self.0.collect(metrics)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn temporality(&self, kind: InstrumentKind) -> Temporality {
        self.0.temporality(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::metrics::v1::{metric::Data, number_data_point};
    use prost::Message;
    use tracing_subscriber::prelude::*;

    use crate::tests::{Wire, attribute, settings};

    #[test]
    fn operation_completion_submits_without_another_request() {
        const CHILD: &str = "RAPIRA_OTEL_OPERATION_METRICS_TEST";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "metrics::tests::operation_completion_submits_without_another_request",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        struct Case {
            name: &'static str,
            operation: &'static str,
            response_finished: bool,
            elapsed: Duration,
            sum: f64,
        }
        let cases = [
            Case {
                name: "queue completion before a response",
                operation: "queue.wait",
                response_finished: false,
                elapsed: Duration::from_millis(125),
                sum: 0.125,
            },
            Case {
                name: "PHP completion after an early response",
                operation: "php.execute",
                response_finished: true,
                elapsed: Duration::from_millis(250),
                sum: 0.25,
            },
        ];
        for case in cases {
            let wire = Wire::new();
            init(Resource::builder_empty().build(), wire.sender.clone());
            if case.response_finished {
                request_finished("GET", 200, Duration::from_secs(1));
            }
            record_duration(case.operation, case.elapsed);
            let operation = wire
                .records()
                .into_iter()
                .filter(|(signal, _)| *signal == 3)
                .flat_map(|(_, bytes)| {
                    ExportMetricsServiceRequest::decode(bytes.as_slice())
                        .unwrap()
                        .resource_metrics
                })
                .flat_map(|resource| resource.scope_metrics)
                .flat_map(|scope| scope.metrics)
                .find(|metric| metric.name == "rapira.operation.duration")
                .unwrap_or_else(|| {
                    panic!("{}: operation completion did not submit metrics", case.name)
                });
            let Some(Data::Histogram(histogram)) = operation.data else {
                panic!("{}: operation histogram missing", case.name);
            };
            assert_eq!(operation.unit, "s", "{}", case.name);
            assert_eq!(histogram.aggregation_temporality, 2, "{}", case.name);
            assert_eq!(histogram.data_points.len(), 1, "{}", case.name);
            let point = &histogram.data_points[0];
            assert_eq!(point.count, 1, "{}", case.name);
            assert_eq!(point.sum, Some(case.sum), "{}", case.name);
            assert_eq!(
                attribute(&point.attributes, "rapira.operation"),
                Some(&Value::StringValue(case.operation.into())),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn request_snapshots_export_values_and_reset_after_fork() {
        const CHILD: &str = "RAPIRA_OTEL_METRICS_TEST";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "metrics::tests::request_snapshots_export_values_and_reset_after_fork",
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
        let mut config = settings();
        config.metrics = true;
        config.traces = false;
        config.logs = true;
        let subscriber =
            tracing_subscriber::registry().with(crate::layer(&config, wire.sender.clone()));
        tracing::subscriber::with_default(subscriber, || {
            record_duration("inherited.operation", Duration::from_secs(10));
            crate::after_fork("worker-test");
            tracing::info!(message = "x".repeat(crate::ipc::MAX_RECORD_BYTES + 1));
            record_duration("php.execute", Duration::from_millis(250));
            request_finished("CUSTOM", 503, Duration::from_secs(2));
            record_duration("php.execute", Duration::from_millis(750));
            request_finished("CUSTOM", 503, Duration::from_secs(3));
        });
        let snapshots: Vec<_> = wire
            .records()
            .into_iter()
            .filter(|(signal, _)| *signal == 3)
            .map(|(_, bytes)| ExportMetricsServiceRequest::decode(bytes.as_slice()).unwrap())
            .collect();
        assert_eq!(
            snapshots.len(),
            5,
            "one snapshot per completed operation or request"
        );
        struct Case {
            name: &'static str,
            index: usize,
            count: u64,
            request_sum: f64,
            operation_sum: f64,
        }
        let cases = [
            Case {
                name: "first request",
                index: 2,
                count: 1,
                request_sum: 2.0,
                operation_sum: 0.25,
            },
            Case {
                name: "cumulative second request",
                index: 4,
                count: 2,
                request_sum: 5.0,
                operation_sum: 1.0,
            },
        ];
        for case in cases {
            let resource = &snapshots[case.index].resource_metrics[0];
            let attributes = &resource.resource.as_ref().unwrap().attributes;
            assert_eq!(
                attribute(attributes, "service.name"),
                Some(&Value::StringValue("sdk-test".into())),
                "{}",
                case.name
            );
            assert_eq!(
                attribute(attributes, "process.pid"),
                Some(&Value::IntValue(std::process::id() as i64)),
                "{}",
                case.name
            );
            assert_eq!(
                attribute(attributes, "rapira.pool"),
                Some(&Value::StringValue("worker-test".into())),
                "{}",
                case.name
            );
            assert_eq!(
                attribute(attributes, "rapira.role"),
                Some(&Value::StringValue("worker".into())),
                "{}",
                case.name
            );
            let metrics: Vec<_> = resource
                .scope_metrics
                .iter()
                .flat_map(|scope| &scope.metrics)
                .collect();
            let request = metrics
                .iter()
                .find(|metric| metric.name == "http.server.request.duration")
                .unwrap();
            assert_eq!(request.unit, "s");
            let Some(Data::Histogram(request)) = &request.data else {
                panic!("request histogram missing")
            };
            assert_eq!(request.aggregation_temporality, 2);
            assert_eq!(request.data_points.len(), 1);
            let point = &request.data_points[0];
            assert_eq!(point.count, case.count, "{}", case.name);
            assert_eq!(point.sum, Some(case.request_sum), "{}", case.name);
            assert_eq!(
                attribute(&point.attributes, "http.request.method"),
                Some(&Value::StringValue("_OTHER".into()))
            );
            assert_eq!(
                attribute(&point.attributes, "http.response.status_code"),
                Some(&Value::IntValue(503))
            );
            let operation = metrics
                .iter()
                .find(|metric| metric.name == "rapira.operation.duration")
                .unwrap();
            let Some(Data::Histogram(operation)) = &operation.data else {
                panic!("operation histogram missing")
            };
            assert_eq!(
                operation.data_points.len(),
                1,
                "inherited metric state was reset"
            );
            assert_eq!(operation.data_points[0].sum, Some(case.operation_sum));
            assert_eq!(
                attribute(&operation.data_points[0].attributes, "rapira.operation"),
                Some(&Value::StringValue("php.execute".into()))
            );
            let dropped = metrics
                .iter()
                .find(|metric| metric.name == "rapira.otel.dropped_records")
                .unwrap();
            let Some(Data::Sum(dropped)) = &dropped.data else {
                panic!("drop counter missing")
            };
            assert!(dropped.is_monotonic);
            assert_eq!(
                dropped.data_points[0].value,
                Some(number_data_point::Value::AsInt(1)),
                "{}",
                case.name
            );
        }
    }
}
