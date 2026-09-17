use crate::harness::*;
use opentelemetry_proto::tonic::collector::{
    logs::v1::ExportLogsServiceRequest, metrics::v1::ExportMetricsServiceRequest,
    trace::v1::ExportTraceServiceRequest,
};
use opentelemetry_proto::tonic::{common::v1::any_value::Value, metrics::v1::metric::Data};
use prost::Message;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);
const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

struct Collector {
    addr: SocketAddr,
    records: Option<mpsc::Receiver<Export>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct Export {
    path: String,
    body: Vec<u8>,
    response: Option<mpsc::Sender<u16>>,
}

impl Export {
    fn respond(&mut self, status: u16) {
        if let Some(response) = self.response.take() {
            response.send(status).unwrap();
        }
    }

    fn spans(&self, trace_id: &[u8]) -> Vec<opentelemetry_proto::tonic::trace::v1::Span> {
        if self.path != "/v1/traces" {
            return Vec::new();
        }
        ExportTraceServiceRequest::decode(self.body.as_slice())
            .unwrap()
            .resource_spans
            .into_iter()
            .flat_map(|r| r.scope_spans)
            .flat_map(|s| s.spans)
            .filter(|s| s.trace_id == trace_id)
            .collect()
    }

    fn histogram_points(
        &self,
        name: &str,
    ) -> Vec<opentelemetry_proto::tonic::metrics::v1::HistogramDataPoint> {
        if self.path != "/v1/metrics" {
            return Vec::new();
        }
        ExportMetricsServiceRequest::decode(self.body.as_slice())
            .unwrap()
            .resource_metrics
            .into_iter()
            .flat_map(|resource| resource.scope_metrics)
            .flat_map(|scope| scope.metrics)
            .filter(|metric| metric.name == name)
            .filter_map(|metric| match metric.data {
                Some(Data::Histogram(histogram)) => Some(histogram.data_points),
                _ => None,
            })
            .flatten()
            .collect()
    }
}

impl Collector {
    fn start() -> Self {
        Self::with_controlled_responses(false)
    }

    fn with_controlled_responses(controlled: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, records) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut length = 0;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                let status = if controlled {
                    let (response, rx) = mpsc::channel();
                    if tx
                        .send(Export {
                            path,
                            body,
                            response: Some(response),
                        })
                        .is_err()
                    {
                        break;
                    }
                    match rx.recv() {
                        Ok(status) => status,
                        Err(_) => break,
                    }
                } else {
                    if tx
                        .send(Export {
                            path,
                            body,
                            response: None,
                        })
                        .is_err()
                    {
                        break;
                    }
                    200
                };
                let reply = format!(
                    "HTTP/1.1 {status} Result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                if reader.get_mut().write_all(reply.as_bytes()).is_err() {
                    continue;
                }
            }
        });
        Self {
            addr,
            records: Some(records),
            stop,
            thread: Some(thread),
        }
    }

    fn next(&self, deadline: Instant) -> Export {
        self.records
            .as_ref()
            .unwrap()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("collector must receive telemetry")
    }

    fn traces_for(&self, trace_id: &[u8]) -> Vec<opentelemetry_proto::tonic::trace::v1::Span> {
        let deadline = Instant::now() + TIMEOUT;
        let mut received = Vec::new();
        loop {
            let mut export = self
                .records
                .as_ref()
                .unwrap()
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "collector must receive the request trace: {error}; received {received:?}"
                    )
                });
            received.push((export.path.clone(), export.body.len()));
            export.respond(200);
            let spans = export.spans(trace_id);
            if !spans.is_empty() {
                return spans;
            }
        }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.records.take();
        let _ = TcpStream::connect(self.addr);
        let _ = self.thread.take().unwrap().join();
    }
}

fn trace_bytes() -> Vec<u8> {
    vec![
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47,
        0x36,
    ]
}

#[test]
fn incoming_parent_reaches_php_and_the_collector() {
    let collector = Collector::start();
    let mut srv = spawn_without_rust_log(
        "otel/dispatcher.php",
        1,
        &format!(
            "mode = \"dispatcher\"\n[otel]\nenabled = true\nendpoint = \"http://{}\"\nflush_interval_ms = 100\n[log]\nlevel = \"error\"\n",
            collector.addr
        ),
    );
    let (status, body) = http_get_with_headers(
        srv.addr,
        "/",
        &[("traceparent", PARENT), ("tracestate", "vendor=opaque")],
        TIMEOUT,
    )
    .unwrap();
    assert_eq!(status, 200, "{}", diagnostics(&srv));
    let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let parent = response["traceContext"]["traceparent"]
        .as_str()
        .expect("PHP traceparent");
    assert_eq!(&parent[3..35], TRACE_ID);
    assert_ne!(&parent[36..52], "00f067aa0ba902b7");
    assert_eq!(response["traceContext"]["tracestate"], "vendor=opaque");
    signal(srv.pid(), libc::SIGQUIT);
    assert_eq!(
        srv.wait_exit(TIMEOUT)
            .expect("graceful telemetry drain")
            .code(),
        Some(0)
    );
    let records: Vec<_> = collector.records.as_ref().unwrap().try_iter().collect();
    let spans: Vec<_> = records
        .iter()
        .flat_map(|record| record.spans(&trace_bytes()))
        .collect();
    let server = spans.iter().find(|s| s.kind == 2).unwrap();
    assert_eq!(
        server.parent_span_id,
        [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7]
    );
    let resources: Vec<_> = records
        .iter()
        .filter(|record| record.path == "/v1/traces")
        .flat_map(|record| {
            ExportTraceServiceRequest::decode(record.body.as_slice())
                .unwrap()
                .resource_spans
        })
        .collect();
    struct ProcessCase {
        name: &'static str,
        span: &'static str,
        role: &'static str,
        pid: i64,
    }
    let cases = [
        ProcessCase {
            name: "master initialization",
            span: "php.module.init",
            role: "master",
            pid: i64::from(srv.pid()),
        },
        ProcessCase {
            name: "worker startup",
            span: "worker.boot",
            role: "worker",
            pid: response["pid"].as_i64().unwrap(),
        },
    ];
    for case in cases {
        let resource = resources
            .iter()
            .find(|resource| {
                resource
                    .scope_spans
                    .iter()
                    .any(|scope| scope.spans.iter().any(|span| span.name == case.span))
            })
            .unwrap_or_else(|| panic!("missing {} span", case.name));
        let attributes = &resource.resource.as_ref().unwrap().attributes;
        let value = |name| {
            attributes
                .iter()
                .find(|attribute| attribute.key == name)
                .and_then(|attribute| attribute.value.as_ref())
                .and_then(|value| value.value.as_ref())
        };
        assert_eq!(
            value("process.pid"),
            Some(&Value::IntValue(case.pid)),
            "{}",
            case.name
        );
        assert_eq!(
            value("rapira.role"),
            Some(&Value::StringValue(case.role.into())),
            "{}",
            case.name
        );
    }
    let php = spans
        .iter()
        .find(|span| span.name == "php.execute")
        .unwrap();
    assert_eq!(
        php.span_id,
        u64::from_str_radix(&parent[36..52], 16)
            .unwrap()
            .to_be_bytes()
    );
    let logs: Vec<_> = records
        .iter()
        .filter(|record| record.path == "/v1/logs")
        .flat_map(|record| {
            ExportLogsServiceRequest::decode(record.body.as_slice())
                .unwrap()
                .resource_logs
        })
        .collect();
    let app_log = logs
        .iter()
        .flat_map(|resource| &resource.scope_logs)
        .flat_map(|scope| &scope.log_records)
        .find(|log| {
            log.body.as_ref().and_then(|body| body.value.as_ref())
                == Some(&Value::StringValue("request handled".into()))
        })
        .expect("application log exports below the stderr filter");
    assert_eq!(app_log.trace_id, trace_bytes());
    assert_eq!(app_log.span_id, php.span_id);
    let metrics: Vec<_> = records
        .iter()
        .filter(|record| record.path == "/v1/metrics")
        .flat_map(|record| {
            ExportMetricsServiceRequest::decode(record.body.as_slice())
                .unwrap()
                .resource_metrics
        })
        .flat_map(|resource| resource.scope_metrics)
        .flat_map(|scope| scope.metrics)
        .collect();
    let Some(Data::Histogram(histogram)) = &metrics
        .iter()
        .find(|metric| metric.name == "http.server.request.duration")
        .expect("request duration metric")
        .data
    else {
        panic!("duration must be a histogram")
    };
    assert_eq!(histogram.data_points.len(), 1);
    assert_eq!(histogram.data_points[0].count, 1);
    assert!(histogram.data_points[0].attributes.iter().any(|attribute| {
        attribute.key == "http.response.status_code"
            && attribute
                .value
                .as_ref()
                .and_then(|value| value.value.as_ref())
                == Some(&Value::IntValue(200))
    }));
}

#[test]
fn worker_and_exporter_failures_preserve_request_service() {
    let collector = Collector::with_controlled_responses(true);
    let mut srv = spawn_with_config(
        "shared/echo-worker.php",
        1,
        &format!(
            "mode = \"dispatcher\"\n[otel]\nenabled = true\nlogs = false\nmetrics = false\nendpoint = \"http://{}\"\nflush_interval_ms = 100\n",
            collector.addr
        ),
    );
    let children = wait_workers(&srv, TIMEOUT, "worker and exporter", |p| p.len() == 2);
    let (status, body) =
        http_get_with_headers(srv.addr, "/", &[("traceparent", PARENT)], TIMEOUT).unwrap();
    assert_eq!(status, 200);
    let worker: u32 = std::str::from_utf8(&body)
        .unwrap()
        .strip_prefix("ok:")
        .unwrap()
        .parse()
        .unwrap();
    let exporter = *children.iter().find(|&&pid| pid != worker).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let mut accepted = loop {
        let mut export = collector.next(deadline);
        if !export.spans(&trace_bytes()).is_empty() {
            break export;
        }
        export.respond(200);
    };
    signal(worker, libc::SIGKILL);
    let replacements = wait_workers(&srv, TIMEOUT, "worker replacement", |p| {
        p.len() == 2 && p.contains(&exporter) && !p.contains(&worker)
    });
    accepted.respond(503);
    let mut retried = collector.next(Instant::now() + TIMEOUT);
    assert_eq!(
        retried.body, accepted.body,
        "accepted records survive the worker crash"
    );
    retried.respond(200);

    let replacement = *replacements.iter().find(|&&pid| pid != exporter).unwrap();
    signal(exporter, libc::SIGKILL);
    wait_workers(&srv, TIMEOUT, "exporter replacement", |p| {
        p.len() == 2 && p.contains(&replacement) && !p.contains(&exporter)
    });
    let (status, body) =
        http_get_with_headers(srv.addr, "/", &[("traceparent", PARENT)], TIMEOUT).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, format!("ok:{replacement}").as_bytes());
    assert!(!collector.traces_for(&trace_bytes()).is_empty());
    signal(srv.pid(), libc::SIGQUIT);
    let deadline = Instant::now() + TIMEOUT;
    while srv.try_status().is_none() {
        assert!(
            Instant::now() < deadline,
            "exporter must drain before shutdown"
        );
        match collector
            .records
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_millis(100))
        {
            Ok(mut export) => export.respond(200),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("collector stopped: {error}"),
        }
    }
}

#[test]
fn completed_response_exports_before_the_next_request() {
    let collector = Collector::start();
    let srv = spawn_without_rust_log(
        "shared/echo-worker.php",
        1,
        &format!(
            "mode = \"dispatcher\"\n[otel]\nenabled = true\nendpoint = \"http://{}\"\nflush_interval_ms = 100\n[log]\nlevel = \"error\"\n",
            collector.addr
        ),
    );
    assert_eq!(
        http_get_with_headers(srv.addr, "/", &[("traceparent", PARENT)], TIMEOUT)
            .unwrap()
            .0,
        200
    );
    let mut spans = collector.traces_for(&trace_bytes());
    let deadline = Instant::now() + TIMEOUT;
    while !spans.iter().any(|span| span.kind == 2) {
        assert!(Instant::now() < deadline, "missing completed server span");
        spans.extend(collector.traces_for(&trace_bytes()));
    }
}

#[test]
fn request_operations_continue_through_middleware_and_multipart() {
    let collector = Collector::start();
    let root = scratch_dir();
    let srv = spawn_with_http_extra(
        "otel/dispatcher.php",
        1,
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n[otel]\nenabled = true\nendpoint = \"http://{}\"\nflush_interval_ms = 100\n",
            root.display(),
            collector.addr
        ),
    );
    let body = "--fields\r\nContent-Disposition: form-data; name=\"value\"\r\n\r\npayload\r\n--fields--\r\n";
    let request = format!(
        "POST /missing HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\ntraceparent: {PARENT}\r\nContent-Type: multipart/form-data; boundary=fields\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    assert_eq!(
        http_raw(srv.addr, request.as_bytes(), TIMEOUT).unwrap().0,
        200,
        "{}",
        diagnostics(&srv)
    );
    let mut spans = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    while !spans
        .iter()
        .any(|span: &opentelemetry_proto::tonic::trace::v1::Span| span.kind == 2)
    {
        assert!(Instant::now() < deadline, "server span must complete");
        spans.extend(collector.traces_for(&trace_bytes()));
    }
    for name in [
        "http.admission",
        "http.middleware",
        "http.request.body",
        "http.multipart",
        "http.dispatch",
        "php.execute",
        "http.response",
    ] {
        assert!(
            spans.iter().any(|span| span.name == name),
            "missing {name}: {:?}",
            spans.iter().map(|span| &span.name).collect::<Vec<_>>()
        );
    }
    let server = spans.iter().find(|span| span.kind == 2).unwrap();
    for span in spans.iter().filter(|span| span.kind != 2) {
        assert!(
            span.parent_span_id == server.span_id
                || spans
                    .iter()
                    .any(|parent| parent.span_id == span.parent_span_id),
            "{} has no local parent",
            span.name
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn php_context_scopes_are_per_request() {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        mode: &'static str,
        retained: &'static str,
    }
    let cases = [
        Case {
            name: "worker callback",
            fixture: "otel/worker.php",
            mode: "worker",
            retained: "previous",
        },
        Case {
            name: "dispatcher lazy request",
            fixture: "otel/snapshot-dispatcher.php",
            mode: "dispatcher",
            retained: "previousLazy",
        },
    ];
    for case in cases {
        let collector = Collector::start();
        let mut srv = spawn_without_rust_log(
            case.fixture,
            1,
            &format!(
                "mode = \"{}\"\n[otel]\nenabled = true\nendpoint = \"http://{}\"\nflush_interval_ms = 100\n[log]\nlevel = \"error\"\n",
                case.mode, collector.addr
            ),
        );
        let request = |headers: &[(&str, &str)]| {
            let (status, body) = http_get_with_headers(srv.addr, "/", headers, TIMEOUT).unwrap();
            assert_eq!(status, 200, "{}: {}", case.name, diagnostics(&srv));
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        };
        let first = request(&[("traceparent", PARENT)]);
        let first_parent = first["active"]["traceparent"].as_str().unwrap();
        assert_eq!(&first_parent[3..35], TRACE_ID, "{}", case.name);
        assert_eq!(first["outside"], serde_json::json!([]), "{}", case.name);
        let second = request(&[(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
        )]);
        let second_parent = second["active"]["traceparent"].as_str().unwrap();
        assert_eq!(&second_parent[3..35], TRACE_ID, "{}", case.name);
        assert_eq!(&second_parent[53..], "00", "{}", case.name);
        assert_ne!(
            &first_parent[36..52],
            &second_parent[36..52],
            "{}",
            case.name
        );
        assert_eq!(second[case.retained], first["active"], "{}", case.name);
        assert_eq!(second["outside"], serde_json::json!([]), "{}", case.name);
        let third = request(&[]);
        let third_parent = third["active"]["traceparent"].as_str().unwrap();
        assert_ne!(&third_parent[3..35], TRACE_ID, "{}", case.name);
        assert_eq!(third[case.retained], second["active"], "{}", case.name);
        assert_eq!(third["outside"], serde_json::json!([]), "{}", case.name);
        signal(srv.pid(), libc::SIGQUIT);
        assert_eq!(
            srv.wait_exit(TIMEOUT).unwrap().code(),
            Some(0),
            "{}",
            case.name
        );
        let records: Vec<_> = collector.records.as_ref().unwrap().try_iter().collect();
        let spans: Vec<_> = records
            .iter()
            .flat_map(|record| record.spans(&trace_bytes()))
            .collect();
        assert!(
            spans.iter().any(|span| span.name == "php.execute"
                && span.span_id
                    == u64::from_str_radix(&first_parent[36..52], 16)
                        .unwrap()
                        .to_be_bytes()),
            "{}",
            case.name
        );
        assert!(
            !spans.iter().any(|span| span.span_id
                == u64::from_str_radix(&second_parent[36..52], 16)
                    .unwrap()
                    .to_be_bytes()),
            "{}: unsampled execution exported",
            case.name
        );
    }
}

#[test]
fn sendfile_uses_one_span_per_transfer() {
    struct Case {
        name: &'static str,
        bytes: usize,
    }
    let cases = [
        Case {
            name: "four reads",
            bytes: 256 * 1024,
        },
        Case {
            name: "one read",
            bytes: 1024,
        },
    ];
    for case in cases {
        let collector = Collector::start();
        let mut srv = spawn_without_rust_log(
            "lifecycle/stream-worker.php",
            1,
            &format!(
                "mode = \"dispatcher\"\n[otel]\nenabled = true\nlogs = false\nmetrics = false\nendpoint = \"http://{}\"\nflush_interval_ms = 100\n",
                collector.addr
            ),
        );
        let path = srv.dir.join("payload.bin");
        let payload = vec![b'x'; case.bytes];
        std::fs::write(&path, &payload).unwrap();
        let (status, body) = http_get_with_headers(
            srv.addr,
            "/?probe=sendfile",
            &[("traceparent", PARENT), ("x-path", path.to_str().unwrap())],
            TIMEOUT,
        )
        .unwrap();
        assert_eq!(status, 200, "{}: {}", case.name, diagnostics(&srv));
        assert_eq!(body, payload, "{}", case.name);
        signal(srv.pid(), libc::SIGQUIT);
        assert_eq!(
            srv.wait_exit(TIMEOUT).unwrap().code(),
            Some(0),
            "{}",
            case.name
        );
        let spans: Vec<_> = collector
            .records
            .as_ref()
            .unwrap()
            .try_iter()
            .flat_map(|record| record.spans(&trace_bytes()))
            .filter(|span| span.name.starts_with("http.sendfile"))
            .collect();
        assert_eq!(
            spans.len(),
            1,
            "{}: one span covers the transfer",
            case.name
        );
        assert_eq!(spans[0].name, "http.sendfile", "{}", case.name);
        assert!(
            spans[0].attributes.iter().any(|attribute| {
                attribute.key == "bytes"
                    && attribute
                        .value
                        .as_ref()
                        .and_then(|value| value.value.as_ref())
                        == Some(&Value::IntValue(case.bytes as i64))
            }),
            "{}: transfer byte count",
            case.name
        );
    }
}

#[test]
fn disabled_telemetry_keeps_an_empty_php_carrier() {
    let srv = spawn_without_rust_log(
        "otel/dispatcher.php",
        1,
        "mode = \"dispatcher\"\n[otel]\nenabled = false\n",
    );
    let (status, body) =
        http_get_with_headers(srv.addr, "/", &[("traceparent", PARENT)], TIMEOUT).unwrap();
    assert_eq!(status, 200);
    let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["traceContext"], serde_json::json!([]));
    let worker = response["pid"].as_u64().unwrap() as u32;
    assert_eq!(worker_pids(srv.pid()), [worker]);
}

#[test]
fn dispatcher_shutdown_completion_keeps_active_trace_context() {
    struct Case {
        name: &'static str,
        path: &'static str,
        status: u16,
        error: bool,
    }
    let cases = [
        Case {
            name: "normal shutdown response",
            path: "/",
            status: 200,
            error: false,
        },
        Case {
            name: "fatal shutdown response",
            path: "/fatal",
            status: 500,
            error: true,
        },
    ];
    for case in cases {
        let collector = Collector::start();
        let mut srv = spawn_without_rust_log(
            "otel/shutdown-dispatcher.php",
            1,
            &format!(
                "mode = \"dispatcher\"\n[otel]\nenabled = true\nmetrics = false\nendpoint = \"http://{}\"\nflush_interval_ms = 100\n[log]\nlevel = \"error\"\n",
                collector.addr
            ),
        );
        let (status, body) =
            http_get_with_headers(srv.addr, case.path, &[("traceparent", PARENT)], TIMEOUT)
                .unwrap();
        assert_eq!(status, case.status, "{}: {}", case.name, diagnostics(&srv));
        let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let parent = response["stored"]["traceparent"].as_str().unwrap();
        assert_eq!(&parent[3..35], TRACE_ID, "{}", case.name);
        assert_eq!(
            response["active"], response["stored"],
            "{}: active carrier during shutdown",
            case.name
        );
        let mut records = Vec::new();
        let mut executed = false;
        let mut finalized = false;
        let deadline = Instant::now() + TIMEOUT;
        while !executed || !finalized {
            let record = collector.next(deadline);
            executed |= record
                .spans(&trace_bytes())
                .iter()
                .any(|span| span.name == "php.execute");
            if record.path == "/v1/logs" {
                finalized |= ExportLogsServiceRequest::decode(record.body.as_slice())
                    .unwrap()
                    .resource_logs
                    .into_iter()
                    .flat_map(|resource| resource.scope_logs)
                    .flat_map(|scope| scope.log_records)
                    .any(|log| {
                        log.body.as_ref().and_then(|body| body.value.as_ref())
                            == Some(&Value::StringValue("dispatcher-shutdown-finished".into()))
                    });
            }
            records.push(record);
        }
        signal(srv.pid(), libc::SIGQUIT);
        assert_eq!(
            srv.wait_exit(TIMEOUT).unwrap().code(),
            Some(0),
            "{}",
            case.name
        );
        let spans: Vec<_> = records
            .iter()
            .flat_map(|record| record.spans(&trace_bytes()))
            .collect();
        let php = spans
            .iter()
            .find(|span| span.name == "php.execute")
            .expect("shutdown execution span");
        assert_eq!(
            php.span_id,
            u64::from_str_radix(&parent[36..52], 16)
                .unwrap()
                .to_be_bytes(),
            "{}",
            case.name
        );
        assert_eq!(
            php.status.as_ref().is_some_and(|status| status.code == 2),
            case.error,
            "{}: execution error status",
            case.name
        );
        let logs: Vec<_> = records
            .iter()
            .filter(|record| record.path == "/v1/logs")
            .flat_map(|record| {
                ExportLogsServiceRequest::decode(record.body.as_slice())
                    .unwrap()
                    .resource_logs
            })
            .flat_map(|resource| resource.scope_logs)
            .flat_map(|scope| scope.log_records)
            .collect();
        let log = |message: &str| {
            logs.iter()
                .find(|log| {
                    log.body.as_ref().and_then(|body| body.value.as_ref())
                        == Some(&Value::StringValue(message.into()))
                })
                .unwrap_or_else(|| panic!("{}: missing {message}", case.name))
        };
        let active = log("dispatcher-shutdown-start");
        assert_eq!(active.trace_id, trace_bytes(), "{}", case.name);
        assert_eq!(active.span_id, php.span_id, "{}", case.name);
        let finished = log("dispatcher-shutdown-finished");
        assert!(
            finished.trace_id.is_empty() && finished.span_id.is_empty(),
            "{}: scope after finalization",
            case.name
        );
        assert!(
            finished.attributes.iter().any(|attribute| {
                attribute.key == "context"
                    && attribute
                        .value
                        .as_ref()
                        .and_then(|value| value.value.as_ref())
                        == Some(&Value::StringValue(r#"{"carrier":[]}"#.into()))
            }),
            "{}: carrier after finalization",
            case.name
        );
    }
}

#[test]
fn early_response_completion_exports_the_final_operation_metric() {
    let collector = Collector::start();
    let gate = TcpListener::bind("127.0.0.1:0").unwrap();
    gate.set_nonblocking(true).unwrap();
    let srv = spawn_without_rust_log(
        "otel/early-response-worker.php",
        1,
        &format!(
            "mode = \"worker\"\n[otel]\nenabled = true\nendpoint = \"http://{}\"\nflush_interval_ms = 100\n[log]\nlevel = \"error\"\n",
            collector.addr
        ),
    );
    let gate_addr = gate.local_addr().unwrap().to_string();
    let (status, body) = http_get_with_headers(
        srv.addr,
        "/",
        &[("x-gate", &gate_addr), ("traceparent", PARENT)],
        TIMEOUT,
    )
    .unwrap();
    assert_eq!(
        (status, body.as_slice()),
        (200, b"early response".as_slice()),
        "{}",
        diagnostics(&srv)
    );
    let (mut release, _) = gate
        .accept()
        .expect("PHP connected to the gate before responding");
    release.set_write_timeout(Some(TIMEOUT)).unwrap();
    let is_operation = |point: &opentelemetry_proto::tonic::metrics::v1::HistogramDataPoint,
                        name: &str| {
        point.attributes.iter().any(|attribute| {
            attribute.key == "rapira.operation"
                && attribute
                    .value
                    .as_ref()
                    .and_then(|value| value.value.as_ref())
                    == Some(&Value::StringValue(name.into()))
        })
    };
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let record = collector.next(deadline);
        assert!(
            !record
                .histogram_points("rapira.operation.duration")
                .iter()
                .any(|point| is_operation(point, "php.execute")),
            "PHP execution must wait for the gate"
        );
        if record
            .histogram_points("http.server.request.duration")
            .iter()
            .any(|point| point.count == 1)
        {
            break;
        }
    }
    release.write_all(b"1").unwrap();
    drop(release);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let record = collector
            .records
            .as_ref()
            .unwrap()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("PHP completion must submit its operation metric without another HTTP request");
        if let Some(point) = record
            .histogram_points("rapira.operation.duration")
            .iter()
            .find(|point| is_operation(point, "php.execute"))
        {
            assert_eq!(point.count, 1);
            break;
        }
    }
}
