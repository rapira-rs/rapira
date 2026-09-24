use extension_api::{Reply, ReplyEvent};
use http::HeaderMap;
use php_sys::{
    Frame, GrpcMethod, GrpcOutcome, GrpcProtocol, GrpcRequest, GrpcService, HandleError, Mode,
    Rapira, RapiraHandle, Request,
};
use std::env::set_var;
use std::path::{Path, PathBuf};
use std::sync::{self, Mutex, Once, OnceLock, PoisonError};
use tokio::sync::{mpsc, oneshot};

static PHP_LOCK: Mutex<()> = Mutex::new(());
static PHP_ENV: Once = Once::new();
static PHP_LOCK_ASYNC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Absolute path to a PHP fixture shipped with this crate (robust to the test's cwd).
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

pub fn php_lock() -> sync::MutexGuard<'static, ()> {
    init_php_env();
    PHP_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn set_phprc(_php: &sync::MutexGuard<'static, ()>, ini: &Path) {
    // SAFETY: PHP_LOCK is held while `_php` lives, so nothing reads the environment concurrently.
    unsafe { set_var("PHPRC", ini) };
}

/// `php_lock()` plus a PHPRC override for this whole test binary.
pub fn php_lock_with_ini(ini: &Path) -> sync::MutexGuard<'static, ()> {
    let guard = php_lock();
    set_phprc(&guard, ini);
    guard
}

/// One resident worker serves every request; `ini` overrides PHPRC for this whole test binary, like [`php_lock_with_ini`].
pub fn run_worker(
    name: &str,
    uris: &[&str],
    ini: Option<&Path>,
) -> anyhow::Result<Vec<(u16, String)>> {
    let guard = php_lock();
    if let Some(ini) = ini {
        set_phprc(&guard, ini);
    }
    let r = Rapira::start(Mode::Worker(fixture(name)))?;
    let h = r.handle();
    let mut out = Vec::with_capacity(uris.len());
    for uri in uris {
        out.push(drain(submit(&h, req(uri, name))?));
    }
    drop(h);
    drop(r);
    Ok(out)
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("tokio runtime")
    })
}

/// Submits `req` through the async intake of `h` and blocks until the job is queued.
pub fn submit(h: &RapiraHandle, req: Request) -> Result<mpsc::Receiver<Frame>, HandleError> {
    runtime().block_on(h.handle(req))
}

/// A unary call to `rapira.test.v1.EchoService/Echo` from a gRPC client on 127.0.0.1, with `message` as the request bytes.
pub fn grpc_request(message: &str) -> GrpcRequest {
    GrpcRequest {
        method: "rapira.test.v1.EchoService/Echo".into(),
        protocol: GrpcProtocol::Grpc,
        metadata: HeaderMap::new(),
        deadline: None,
        remote: php_sys::types::Addr::Inet(([127, 0, 0, 1], 50051).into()),
        message: message.as_bytes().to_vec().into(),
    }
}

/// Submits `req` through the async intake of `h` and blocks until the call is queued.
pub fn call(
    h: &RapiraHandle,
    req: GrpcRequest,
) -> Result<oneshot::Receiver<GrpcOutcome>, HandleError> {
    runtime().block_on(h.call(req))
}

/// Waits at most 10 s for the outcome of a call. None means that the call was lost: PHP dropped it unfinalized.
pub fn outcome(rx: oneshot::Receiver<GrpcOutcome>) -> Option<GrpcOutcome> {
    runtime().block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("no outcome within 10 s")
            .ok()
    })
}

/// Panics when RAPIRA_REQUIRE_EXTS names an extension this fixture covers: a skip where CI installs the extension is a broken install.
pub fn assert_skip_allowed(fixture: &str) {
    let Ok(required) = std::env::var("RAPIRA_REQUIRE_EXTS") else {
        return;
    };
    for ext in required.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        assert!(
            !fixture.contains(ext),
            "{fixture} skipped, but RAPIRA_REQUIRE_EXTS demands {ext}"
        );
    }
}

/// Build a minimal `GET` request for `uri`, and point the worker script paths at `fixture_name`.
pub fn req(uri: &str, fixture_name: &str) -> Request {
    php_sys::set_script(&fixture(fixture_name));
    Request {
        https: false,
        method: "GET".into(),
        uri: uri.into(),
        target: None,
        authority: None,
        protocol: "HTTP/1.1".into(),
        remote: php_sys::types::Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server: php_sys::types::Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server_name: "localhost".into(),
        server_port: 8080,
        headers: HeaderMap::new(),
        content_type: None,
        content_length: 0,
        body: php_sys::types::Body::Raw(std::io::Cursor::new(Vec::new())),
        received_at: None,
        tls: None,
    }
}

/// The FileDescriptorSet of `crates/plugins/grpc/testdata/echo.proto`.
pub fn echo_descriptor_set() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/grpc/testdata/echo.binpb")
}

/// The services of `crates/plugins/grpc/testdata/echo.proto`, in descriptor order.
pub fn echo_services() -> Vec<GrpcService> {
    let method = |name: &str, server_streaming: bool| GrpcMethod {
        name: name.into(),
        input_type: "rapira.test.v1.EchoRequest".into(),
        output_type: "rapira.test.v1.EchoResponse".into(),
        client_streaming: false,
        server_streaming,
    };
    vec![GrpcService {
        name: "rapira.test.v1.EchoService".into(),
        methods: vec![
            method("Echo", false),
            method("Get", false),
            method("Watch", true),
        ],
    }]
}

/// A response stream collected to its `End` (or to the producer dying).
#[derive(Default)]
pub struct Resp {
    pub interim: Vec<php_sys::ResponseHead>,
    pub head: Option<php_sys::ResponseHead>,
    pub content_length: Option<u64>,
    pub bodiless: bool,
    pub body: Vec<u8>,
    pub trailers: HeaderMap,
    pub truncated: bool,
    /// An `End` frame arrived; false = the producer died first.
    pub ended: bool,
    /// Head frames seen; `head` keeps only the last, so a duplicate would otherwise be invisible.
    pub heads: u32,
}

impl Resp {
    /// 0 = no head (producer died, or the response never recorded one).
    pub fn status(&self) -> u16 {
        self.head.as_ref().map_or(0, |h| h.status)
    }

    pub fn header(&self, name: &str) -> Option<String> {
        let value = self.head.as_ref()?.headers.get(name)?;
        Some(String::from_utf8_lossy(value.as_bytes()).into_owned())
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Fold one frame in; true when the stream is over.
    fn fold(&mut self, frame: Frame) -> bool {
        match frame {
            Frame::Interim(h) => self.interim.push(h),
            Frame::Head {
                head,
                content_length,
                bodiless,
                ..
            } => {
                self.heads += 1;
                self.head = Some(head);
                self.content_length = content_length;
                self.bodiless = bodiless;
            }
            Frame::Chunk(b) => self.body.extend_from_slice(&b),
            Frame::File { file, offset, len } => match read_slice(&file, offset, len) {
                Ok(bytes) => self.body.extend_from_slice(&bytes),
                Err(e) => panic!("reading a File frame: {e}"),
            },
            Frame::End {
                trailers,
                truncated,
            } => {
                self.trailers = trailers;
                self.truncated = truncated;
                self.ended = true;
                return true;
            }
        }
        false
    }
}

fn read_slice(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut out = vec![0u8; usize::try_from(len).unwrap_or(usize::MAX)];
    let mut done = 0usize;
    while done < out.len() {
        let n = file.read_at(&mut out[done..], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    Ok(out)
}

/// A reply stream collected to its `End`.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// Drains `reply` to its `End`; a missing head, a missing `End` or a truncated `End` is an error.
pub async fn collect(mut reply: Reply) -> anyhow::Result<Response> {
    let mut response: Option<Response> = None;
    let mut end: Option<bool> = None;
    while let Some(ev) = reply.next().await {
        match ev {
            ReplyEvent::Interim { .. } => {}
            ReplyEvent::Head {
                status, headers, ..
            } => {
                response = Some(Response {
                    status,
                    headers,
                    body: Vec::new(),
                });
            }
            ReplyEvent::Chunk(b) => {
                if let Some(r) = response.as_mut() {
                    r.body.extend_from_slice(&b);
                }
            }
            ReplyEvent::File { file, offset, len } => {
                if let Some(r) = response.as_mut() {
                    r.body.extend_from_slice(&read_slice(&file, offset, len)?);
                }
            }
            ReplyEvent::End { truncated, .. } => {
                end = Some(truncated);
                break;
            }
        }
    }
    match (response, end) {
        (None, None) => Err(anyhow::anyhow!(
            "php worker died mid-response (channel closed without a response)"
        )),
        (Some(_), None) | (_, Some(true)) => {
            Err(anyhow::anyhow!("php crashed mid-response; body truncated"))
        }
        (None, Some(false)) => Err(anyhow::anyhow!("php produced no response head")),
        (Some(r), Some(false)) => Ok(r),
    }
}

/// Poll for a first frame until `deadline`: None = nothing arrived in time, a producer that died with no frames yields `Resp::default()`.
pub fn drain_resp_deadline(
    rx: &mut mpsc::Receiver<Frame>,
    deadline: std::time::Instant,
) -> Option<Resp> {
    loop {
        match rx.try_recv() {
            Ok(frame) => {
                let mut resp = Resp::default();
                if !resp.fold(frame) {
                    while let Some(f) = rx.blocking_recv() {
                        if resp.fold(f) {
                            break;
                        }
                    }
                }
                return Some(resp);
            }
            Err(mpsc::error::TryRecvError::Disconnected) => return Some(Resp::default()),
            Err(mpsc::error::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

pub fn drain_resp(mut rx: mpsc::Receiver<Frame>) -> Resp {
    let mut resp = Resp::default();
    while let Some(frame) = rx.blocking_recv() {
        if resp.fold(frame) {
            break;
        }
    }
    resp
}

/// Drain to `(status, body)`; status is 0 when no head was produced.
pub fn drain(rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let r = drain_resp(rx);
    (r.status(), r.body_string())
}

pub async fn php_lock_async() -> tokio::sync::MutexGuard<'static, ()> {
    init_php_env();
    PHP_LOCK_ASYNC.lock().await
}

pub async fn drain_resp_async(mut rx: mpsc::Receiver<Frame>) -> Resp {
    let mut resp = Resp::default();
    while let Some(frame) = rx.recv().await {
        if resp.fold(frame) {
            break;
        }
    }
    resp
}

pub async fn drain_async(rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let r = drain_resp_async(rx).await;
    (r.status(), r.body_string())
}

/// Sets PHPRC once, before any `Rapira::start`: https://www.php.net/manual/en/configuration.file.php
fn init_php_env() {
    PHP_ENV.call_once(|| {
        // SAFETY: the Once runs this exactly once, before any Rapira::start / php_module_startup.
        unsafe {
            set_var(
                "PHPRC",
                concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/ini/shared/php.ini"),
            );
        }
    });
}

/// One captured record. PHP diagnostics are asserted on level and target, not just text.
#[derive(Debug)]
pub struct Captured {
    pub level: tracing::Level,
    pub target: String,
    pub message: String,
    /// The `context` field, empty when the record carries none; Rapira\log() puts its JSON-encoded context array here.
    pub context: String,
}

static LOG_CAPTURE: Mutex<Vec<Captured>> = Mutex::new(Vec::new());

/// The captured records; the lock is poison-recovered so one failing assertion cannot make every later test in the binary die on `PoisonError`.
pub fn captured() -> sync::MutexGuard<'static, Vec<Captured>> {
    LOG_CAPTURE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Collects the `message` and `context` fields; other fields are ignored.
#[derive(Default)]
struct Msg {
    message: String,
    context: String,
}

impl Msg {
    fn slot(&mut self, name: &str) -> Option<&mut String> {
        match name {
            "message" => Some(&mut self.message),
            "context" => Some(&mut self.context),
            _ => None,
        }
    }
}

impl tracing::field::Visit for Msg {
    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
        if let Some(slot) = self.slot(f.name()) {
            *slot = v.to_owned();
        }
    }
    // A `%value` field lands here: tracing wraps Display in a format_args! whose Debug forwards to it.
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        if let Some(slot) = self.slot(f.name()) {
            *slot = format!("{v:?}");
        }
    }
}

struct CaptureLayer;

impl<S: tracing::Subscriber> tracing_subscriber::layer::Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let mut msg = Msg::default();
        event.record(&mut msg);
        captured().push(Captured {
            level: *meta.level(),
            target: meta.target().to_owned(),
            message: msg.message,
            context: msg.context,
        });
    }
}

/// One `app`-target record left by `\Rapira\log()`: level, message, context JSON.
pub type AppRecord = (tracing::Level, String, String);

/// Runs `script` in classic mode and returns its `app`-target records and its `php`-target messages, both read under the PHP lock; the fixture must echo `logged` last, so a script that died half way cannot masquerade as one that logged nothing.
pub fn app_records(script: &str) -> (Vec<AppRecord>, Vec<String>) {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Classic).expect("classic boot");
    let h = r.handle();
    let (status, body) = drain(submit(&h, req("/", script)).expect("dispatch"));
    drop(h);
    drop(r);

    assert_eq!(status, 200, "{script} must run clean (body: {body:?})");
    assert!(body.contains("logged"), "{script} ran to the end: {body:?}");

    let all = captured();
    let app = all
        .iter()
        .filter(|c| c.target == "app")
        .map(|c| (c.level, c.message.clone(), c.context.clone()))
        .collect();
    let php = all
        .iter()
        .filter(|c| c.target == "php")
        .map(|c| c.message.clone())
        .collect();
    (app, php)
}

/// The one `app` record `script` must leave; asserting the count fails the test on a stray extra record instead of ignoring it.
pub fn app_record(script: &str) -> AppRecord {
    let (records, _) = app_records(script);
    assert_eq!(
        records.len(),
        1,
        "{script} must log exactly one app record (got {records:?})"
    );
    records.into_iter().next().expect("checked above")
}

/// Polls the captured records until an `app` record with `message` appears, for at most 10 s; returns its context.
pub fn wait_app_record(message: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(ctx) = captured()
            .iter()
            .find(|c| c.target == "app" && c.message == message)
            .map(|c| c.context.clone())
        {
            return ctx;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no {message:?} app record within 10s"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Installs the capturing subscriber once, unfiltered, so even trace-level records reach `LOG_CAPTURE`.
pub fn init_log_capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let _ = tracing_subscriber::registry().with(CaptureLayer).try_init();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_api::ReplySource;

    struct VecSource(std::collections::VecDeque<ReplyEvent>);

    impl ReplySource for VecSource {
        fn poll_next(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<ReplyEvent>> {
            std::task::Poll::Ready(self.0.pop_front())
        }
    }

    fn reply(events: Vec<ReplyEvent>) -> Reply {
        Reply::new(Box::new(VecSource(events.into())))
    }

    fn fields() -> HeaderMap {
        HeaderMap::from_iter([(
            http::HeaderName::from_static("x-a"),
            http::HeaderValue::from_static("1"),
        )])
    }

    fn head() -> ReplyEvent {
        ReplyEvent::Head {
            status: 200,
            headers: fields(),
            content_length: None,
            bodiless: false,
        }
    }

    fn end(truncated: bool) -> ReplyEvent {
        ReplyEvent::End {
            trailers: HeaderMap::new(),
            truncated,
        }
    }

    /// Each failed stream maps to its error: no events, no `End`, a truncated `End`, no head.
    #[tokio::test]
    async fn collect_maps_stream_outcomes() {
        let died = collect(reply(Vec::new())).await.unwrap_err();
        assert!(died.to_string().contains("died mid-response"), "{died:#}");

        let cut = collect(reply(vec![head()])).await.unwrap_err();
        assert!(cut.to_string().contains("truncated"), "{cut:#}");

        let cut = collect(reply(vec![head(), end(true)])).await.unwrap_err();
        assert!(cut.to_string().contains("truncated"), "{cut:#}");

        let headless = collect(reply(vec![end(false)])).await.unwrap_err();
        assert!(
            headless.to_string().contains("no response head"),
            "{headless:#}"
        );
    }

    /// Chunks concatenate in order; interim heads are dropped.
    #[tokio::test]
    async fn collect_concatenates_the_stream() {
        let r = collect(reply(vec![
            ReplyEvent::Interim {
                status: 103,
                headers: HeaderMap::new(),
            },
            head(),
            ReplyEvent::Chunk(b"one,"[..].into()),
            ReplyEvent::Chunk(b"two"[..].into()),
            end(false),
        ]))
        .await
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.headers, fields());
        assert_eq!(r.body, b"one,two");
    }
}
