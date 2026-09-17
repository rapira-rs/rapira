use crate::ipc::{MAX_RECORD_BYTES, Signal};
use opentelemetry_proto::tonic::collector::{
    logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse},
    metrics::v1::{ExportMetricsServiceRequest, ExportMetricsServiceResponse},
    trace::v1::{ExportTraceServiceRequest, ExportTraceServiceResponse},
};
use prost::Message;
use rapira_config::OtelSettings;
use std::future::Future;
use std::io;
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;
use tokio::io::{AsyncReadExt, unix::AsyncFd};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

struct Record {
    signal: Signal,
    payload: Vec<u8>,
}

#[derive(Default)]
struct Batch {
    traces: ExportTraceServiceRequest,
    logs: ExportLogsServiceRequest,
    metrics: ExportMetricsServiceRequest,
    count: usize,
}

impl Batch {
    fn push(&mut self, record: Record) -> Result<(), prost::DecodeError> {
        match record.signal {
            Signal::Traces => self.traces.resource_spans.extend(
                ExportTraceServiceRequest::decode(record.payload.as_slice())?.resource_spans,
            ),
            Signal::Logs => self
                .logs
                .resource_logs
                .extend(ExportLogsServiceRequest::decode(record.payload.as_slice())?.resource_logs),
            Signal::Metrics => self.metrics.resource_metrics.extend(
                ExportMetricsServiceRequest::decode(record.payload.as_slice())?.resource_metrics,
            ),
        }
        self.count += 1;
        Ok(())
    }

    fn take(&mut self) -> Vec<(Signal, Vec<u8>)> {
        let batch = std::mem::take(self);
        let mut requests = Vec::new();
        if !batch.traces.resource_spans.is_empty() {
            requests.push((Signal::Traces, batch.traces.encode_to_vec()));
        }
        if !batch.logs.resource_logs.is_empty() {
            requests.push((Signal::Logs, batch.logs.encode_to_vec()));
        }
        if !batch.metrics.resource_metrics.is_empty() {
            requests.push((Signal::Metrics, batch.metrics.encode_to_vec()));
        }
        requests
    }
}

struct Client {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    timeout: Duration,
}

impl Client {
    fn new(config: &OtelSettings) -> anyhow::Result<Self> {
        let mut headers = http::HeaderMap::new();
        for (name, value) in &config.headers {
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes())?,
                http::HeaderValue::from_str(value)?,
            );
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .default_headers(headers)
                .build()?,
            endpoint: config.endpoint.parse()?,
            timeout: Duration::from_secs(config.export_timeout_secs),
        })
    }

    async fn export(&self, signal: Signal, body: Vec<u8>) -> anyhow::Result<()> {
        let mut endpoint = self.endpoint.clone();
        let signal_name = match signal {
            Signal::Traces => "traces",
            Signal::Logs => "logs",
            Signal::Metrics => "metrics",
        };
        endpoint.set_path(&format!(
            "{}/v1/{signal_name}",
            endpoint.path().trim_end_matches('/')
        ));
        tokio::time::timeout(self.timeout, async {
            let mut delay = Duration::from_millis(100);
            loop {
                let result = self.http.post(endpoint.clone())
                    .header(http::header::CONTENT_TYPE, "application/x-protobuf")
                    .body(body.clone()).send().await;
                match result {
                    Ok(response) if response.status().is_success() => {
                        let bytes = response.bytes().await?;
                        let rejected = match signal {
                            Signal::Traces => ExportTraceServiceResponse::decode(bytes)?.partial_success.map_or(0, |s| s.rejected_spans),
                            Signal::Logs => ExportLogsServiceResponse::decode(bytes)?.partial_success.map_or(0, |s| s.rejected_log_records),
                            Signal::Metrics => ExportMetricsServiceResponse::decode(bytes)?.partial_success.map_or(0, |s| s.rejected_data_points),
                        };
                        if rejected > 0 { tracing::error!(target: "otel", signal = signal_name, rejected, "collector rejected records"); }
                        return Ok(());
                    }
                    Ok(response) if matches!(response.status().as_u16(), 429 | 502 | 503 | 504) => {
                        let retry = response.headers().get(http::header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok())
                            .map(Duration::from_secs).unwrap_or(delay);
                        tokio::time::sleep(retry.max(delay)).await;
                    }
                    Ok(response) => anyhow::bail!("collector returned {} for {signal_name}", response.status()),
                    Err(error) if error.is_connect() || error.is_timeout() || error.is_request() => tokio::time::sleep(delay).await,
                    Err(error) => return Err(error.into()),
                }
                delay = (delay * 2).min(Duration::from_secs(1));
            }
        }).await?
    }

    async fn flush(&self, batch: &mut Batch) {
        let count = batch.count;
        for (signal, body) in batch.take() {
            if let Err(error) = self.export(signal, body).await {
                tracing::error!(target: "otel", ?signal, records = count, %error, "export failed");
            }
        }
    }
}

async fn receive(stream: UnixStream, sender: mpsc::Sender<Record>) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let mut stream = tokio::net::UnixStream::from_std(stream)?;
    loop {
        let size = match stream.read_u32().await {
            Ok(size) => size as usize,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        if size == 0 || size > MAX_RECORD_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let Ok(permit) = sender.reserve().await else {
            return Ok(());
        };
        let mut frame = vec![0; size];
        if let Err(error) = stream.read_exact(&mut frame).await {
            return if error.kind() == io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(error)
            };
        }
        let signal = match frame.remove(0) {
            1 => Signal::Traces,
            2 => Signal::Logs,
            3 => Signal::Metrics,
            _ => return Err(io::ErrorKind::InvalidData.into()),
        };
        permit.send(Record {
            signal,
            payload: frame,
        });
    }
}

async fn accept(listener: &AsyncFd<UnixListener>) -> io::Result<UnixStream> {
    loop {
        let mut ready = listener.readable().await?;
        match ready.try_io(|fd| fd.get_ref().accept().map(|(stream, _)| stream)) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => retry_accept(error).await?,
            Err(_) => continue,
        }
    }
}

async fn retry_accept(error: io::Error) -> io::Result<()> {
    if matches!(
        error.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK)
    ) {
        return Err(error);
    }
    tracing::warn!(target: "otel", %error, "accept failed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

async fn export_records(
    client: Client,
    config: &OtelSettings,
    mut rx: mpsc::Receiver<Record>,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(config.flush_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    let mut batch = Batch::default();
    loop {
        tokio::select! {
            record = rx.recv() => {
                match record {
                    Some(record) => {
                        batch.push(record)?;
                        if batch.count >= config.batch_size { client.flush(&mut batch).await; }
                    }
                    None => {
                        client.flush(&mut batch).await;
                        return Ok(());
                    }
                }
            }
            _ = interval.tick(), if batch.count > 0 => { client.flush(&mut batch).await; }
        }
    }
}

pub async fn run(
    config: OtelSettings,
    max_connections: usize,
    listener: UnixListener,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let client = Client::new(&config)?;
    listener.set_nonblocking(true)?;
    let listener = AsyncFd::new(listener)?;
    let (tx, rx) = mpsc::channel(config.queue_size);
    let mut readers = JoinSet::new();
    let exporting = export_records(client, &config, rx);
    let accepting = accept(&listener);
    tokio::pin!(shutdown, exporting, accepting);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            stream = &mut accepting, if readers.len() < max_connections => {
                readers.spawn(receive(stream?, tx.clone()));
                accepting.set(accept(&listener));
            }
            Some(_) = readers.join_next(), if !readers.is_empty() => {}
            result = &mut exporting => return result,
        }
    }
    let draining = async {
        loop {
            if readers.len() >= max_connections {
                readers.join_next().await;
                continue;
            }
            match listener.get_ref().accept() {
                Ok((stream, _)) => {
                    readers.spawn(receive(stream, tx.clone()));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => retry_accept(error).await?,
            }
        }
        drop(tx);
        while readers.join_next().await.is_some() {}
        Ok::<(), io::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(config.export_timeout_secs), async {
        tokio::select! {
            result = draining => {
                result?;
                exporting.await
            }
            result = &mut exporting => result,
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("exporter drain timed out"))?
}

pub fn run_process(config: OtelSettings, max_connections: usize) -> anyhow::Result<()> {
    // SAFETY: the parent passes the listener on stdin and its control socket on stdout.
    let (listener, control) = unsafe { (UnixListener::from_raw_fd(0), UnixStream::from_raw_fd(1)) };
    control.set_nonblocking(true)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let mut control = tokio::net::UnixStream::from_std(control)?;
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let mut quit = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())?;
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            // SAFETY: signal handlers are installed before the inherited mask is cleared.
            unsafe {
                let mut empty = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            }
            run(config, max_connections, listener, async move {
                let mut byte = [0];
                tokio::select! {
                    _ = control.read(&mut byte) => {},
                    _ = term.recv() => {},
                    _ = quit.recv() => {},
                    _ = interrupt.recv() => {},
                }
            })
            .await
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::{
        resource::v1::Resource,
        trace::v1::{ResourceSpans, ScopeSpans, Span},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::oneshot;

    struct Export {
        path: String,
        body: Vec<u8>,
        respond: oneshot::Sender<u16>,
    }

    async fn collector() -> (String, mpsc::Receiver<Export>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/prefix", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                let mut length = 0;
                loop {
                    line.clear();
                    stream.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let (respond, response) = oneshot::channel();
                if tx
                    .send(Export {
                        path,
                        body,
                        respond,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                let Ok(status) = response.await else {
                    break;
                };
                let reply = format!(
                    "HTTP/1.1 {status} Result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.get_mut().write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (endpoint, rx, task)
    }

    fn trace_record(name: &str) -> Vec<u8> {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        trace_id: vec![1; 16],
                        span_id: vec![2; 8],
                        name: name.into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec()
    }

    async fn next_export(rx: &mut mpsc::Receiver<Export>) -> Export {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn frame_headers_reserve_queue_capacity_before_payload_reads() {
        use std::io::Write;
        use std::os::fd::AsRawFd;

        struct Case {
            name: &'static str,
            header: &'static [u8],
            remaining: usize,
        }
        let cases = [
            Case {
                name: "idle producer leaves capacity for another producer",
                header: &[],
                remaining: 1,
            },
            Case {
                name: "partial frame owns a queue credit before payload allocation",
                header: &[0, 0, 16, 0],
                remaining: 0,
            },
        ];
        for case in cases {
            let (mut producer, stream) = UnixStream::pair().unwrap();
            let observer = stream.try_clone().unwrap();
            producer.write_all(case.header).unwrap();
            let (tx, mut rx) = mpsc::channel(1);
            let reader = tokio::spawn(receive(stream, tx.clone()));
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    tokio::task::yield_now().await;
                    let mut unread: libc::c_int = 0;
                    // SAFETY: observer is live and unread is a writable integer.
                    assert_eq!(
                        unsafe { libc::ioctl(observer.as_raw_fd(), libc::FIONREAD, &mut unread) },
                        0
                    );
                    if unread == 0 {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(rx.capacity(), case.remaining, "{}", case.name);
            if case.header.is_empty() {
                let (mut active, stream) = UnixStream::pair().unwrap();
                active.write_all(b"\0\0\0\x02\x01x").unwrap();
                let active_reader = tokio::spawn(receive(stream, tx.clone()));
                let record = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(record.payload, b"x", "{}", case.name);
                drop(active);
                active_reader.await.unwrap().unwrap();
            }
            drop(producer);
            reader.await.unwrap().unwrap();
            assert_eq!(rx.capacity(), 1, "{}", case.name);
        }
    }

    #[tokio::test]
    async fn collector_backpressure_bounds_readers_across_producer_reconnects() {
        struct Case {
            name: &'static str,
            max_readers: usize,
            reconnects: usize,
        }
        let cases = [Case {
            name: "one worker reconnects while the collector holds its response",
            max_readers: 3,
            reconnects: 12,
        }];
        for case in cases {
            let (endpoint, mut received, collector) = collector().await;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("export.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let sender = crate::ipc::Sender::new(path);
            let payload = trace_record(&"retained".repeat(128));
            assert!(sender.send(Signal::Traces, &payload));
            let mut config = crate::tests::settings();
            config.endpoint = endpoint;
            config.batch_size = 1;
            config.queue_size = 1;
            config.export_timeout_secs = 30;
            let (stop, shutdown) = oneshot::channel();
            let exporter = tokio::spawn(run(config, case.max_readers, listener, async {
                shutdown.await.unwrap()
            }));
            let first = next_export(&mut received).await;
            let metrics = tokio::runtime::Handle::current().metrics();
            let other_tasks = metrics.num_alive_tasks() - 1;
            let mut accepted = 1;
            for _ in 0..case.reconnects {
                let sender = sender.clone();
                let payload = payload.clone();
                let submitted = tokio::task::spawn_blocking(move || {
                    let mut accepted = 0;
                    while sender.send(Signal::Traces, &payload) {
                        accepted += 1;
                        assert!(accepted < 10_000, "backpressure must drop new records");
                    }
                    accepted
                });
                accepted += tokio::time::timeout(Duration::from_secs(5), submitted)
                    .await
                    .expect("producer submissions must remain nonblocking")
                    .unwrap();
                tokio::task::yield_now().await;
                assert!(
                    metrics.num_alive_tasks() <= other_tasks + case.max_readers,
                    "{}: reader tasks exceeded {}: {}",
                    case.name,
                    case.max_readers,
                    metrics.num_alive_tasks() - other_tasks
                );
            }
            assert!(sender.dropped_records() >= case.reconnects as u64);
            drop(sender);
            stop.send(()).unwrap();
            first.respond.send(200).unwrap();
            for _ in 1..accepted {
                let export = next_export(&mut received).await;
                assert_eq!(export.body, payload, "{}", case.name);
                export.respond.send(200).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(5), exporter)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            collector.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn accept_retries_descriptor_exhaustion_with_backoff() {
        use std::task::Poll;

        const CHILD: &str = "RAPIRA_OTEL_ACCEPT_TEST";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "exporter::tests::accept_retries_descriptor_exhaustion_with_backoff",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accept.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = AsyncFd::new(listener).unwrap();
        let mut producer = UnixStream::connect(&path).unwrap();
        std::io::Write::write_all(&mut producer, b"retained").unwrap();
        let _ = listener.readable().await.unwrap();
        let limit = libc::rlimit {
            rlim_cur: 128,
            rlim_max: 128,
        };
        // SAFETY: this isolated test child owns its descriptor limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut files = Vec::new();
        loop {
            match std::fs::File::open("/dev/null") {
                Ok(file) => files.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                    break;
                }
            }
        }
        let accepting = accept(&listener);
        tokio::pin!(accepting);
        std::future::poll_fn(|cx| {
            assert!(
                accepting.as_mut().poll(cx).is_pending(),
                "descriptor exhaustion must keep the accept future alive"
            );
            Poll::Ready(())
        })
        .await;
        drop(files);
        let mut replacement = UnixStream::connect(path).unwrap();
        std::io::Write::write_all(&mut replacement, b"retained").unwrap();
        tokio::time::advance(Duration::from_millis(99)).await;
        std::future::poll_fn(|cx| {
            assert!(
                accepting.as_mut().poll(cx).is_pending(),
                "accept must wait for its backoff"
            );
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(Duration::from_millis(1)).await;
        let mut stream = tokio::time::timeout(Duration::from_secs(5), accepting)
            .await
            .unwrap()
            .unwrap();
        let mut bytes = [0; 8];
        std::io::Read::read_exact(&mut stream, &mut bytes).unwrap();
        assert_eq!(&bytes, b"retained");
    }

    #[tokio::test]
    async fn transient_failure_retries_the_same_accepted_records() {
        let (endpoint, mut received, collector) = collector().await;
        let mut config = crate::tests::settings();
        config.endpoint = endpoint;
        let client = Client::new(&config).unwrap();
        let payload = trace_record("completed-before-worker-exit");
        let expected = payload.clone();
        let export = tokio::spawn(async move { client.export(Signal::Traces, payload).await });
        let first = next_export(&mut received).await;
        assert_eq!(first.path, "/prefix/v1/traces");
        assert_eq!(first.body, expected);
        first.respond.send(503).unwrap();
        let second = next_export(&mut received).await;
        assert_eq!(second.body, expected);
        second.respond.send(200).unwrap();
        export.await.unwrap().unwrap();
        collector.abort();
    }

    #[tokio::test]
    async fn shutdown_drains_records_from_disconnected_producers() {
        struct Case {
            name: &'static str,
            batch_size: usize,
            batches: &'static [&'static [&'static str]],
        }
        let cases = [
            Case {
                name: "partial final batch",
                batch_size: 16,
                batches: &[&["first", "second"]],
            },
            Case {
                name: "batch limit during drain",
                batch_size: 1,
                batches: &[&["first"], &["second"]],
            },
        ];
        for case in cases {
            let (endpoint, mut received, collector) = collector().await;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("export.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let sender = crate::ipc::Sender::new(path);
            assert!(sender.send(Signal::Traces, &trace_record("first")));
            assert!(sender.send(Signal::Traces, &trace_record("second")));
            drop(sender);
            let mut config = crate::tests::settings();
            config.endpoint = endpoint;
            config.batch_size = case.batch_size;
            config.flush_interval_ms = 60_000;
            let exporter = tokio::spawn(run(config, 3, listener, async {}));
            for expected in case.batches {
                let export = next_export(&mut received).await;
                let request = ExportTraceServiceRequest::decode(export.body.as_slice()).unwrap();
                let names: Vec<_> = request
                    .resource_spans
                    .into_iter()
                    .flat_map(|r| r.scope_spans)
                    .flat_map(|s| s.spans)
                    .map(|span| span.name)
                    .collect();
                assert_eq!(names, *expected, "{}", case.name);
                export.respond.send(200).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(5), exporter)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            collector.abort();
        }
    }

    #[test]
    fn batching_preserves_source_span_identity_and_resource() {
        let span = Span {
            trace_id: vec![
                0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e,
                0x47, 0x36,
            ],
            span_id: vec![1, 2, 3, 4, 5, 6, 7, 8],
            parent_span_id: vec![0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7],
            name: "php.execute".into(),
            start_time_unix_nano: 100,
            end_time_unix_nano: 200,
            ..Default::default()
        };
        let resource = ResourceSpans {
            resource: Some(Resource::default()),
            scope_spans: vec![ScopeSpans {
                spans: vec![span.clone()],
                schema_url: "https://opentelemetry.io/schemas/1.41.0".into(),
                ..Default::default()
            }],
            schema_url: "https://opentelemetry.io/schemas/1.41.0".into(),
        };
        let input = ExportTraceServiceRequest {
            resource_spans: vec![resource.clone()],
        }
        .encode_to_vec();
        let mut batch = Batch::default();
        batch
            .push(Record {
                signal: Signal::Traces,
                payload: input,
            })
            .unwrap();
        let request =
            ExportTraceServiceRequest::decode(batch.take().remove(0).1.as_slice()).unwrap();
        assert_eq!(request.resource_spans, vec![resource]);
        assert_eq!(request.resource_spans[0].scope_spans[0].spans, vec![span]);
    }
}
