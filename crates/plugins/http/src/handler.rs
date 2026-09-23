use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use extension_api::{
    Addr, BoxError, BoxFuture, Handler, HttpRequest, HttpResponse, Middleware, Next, Peer, Php,
    Protocol, Rejected, ReplyEvent,
};
use http_body::Body;
use http_body_util::BodyExt;

use crate::response::{error_response, response_headers};
use crate::{Config, bridge, check, request};

pub(crate) struct Shared {
    pub cfg: Config,
    pub php: Php,
    pub chain: Arc<[Arc<dyn Middleware>]>,
    pub inflight: Arc<AtomicUsize>,
}

pub(crate) struct InflightReqCount {
    counter: Arc<AtomicUsize>,
    /// Connection flush count when the last response byte was handed to hyper.
    /// It lives on the shared guard: the body records it and the drain task reads it.
    pub(crate) end_flush: OnceLock<u64>,
}

impl InflightReqCount {
    pub(crate) fn init(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self {
            counter: Arc::clone(counter),
            end_flush: OnceLock::new(),
        }
    }
}

impl Drop for InflightReqCount {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Per-request values that travel to [`Conn::serve`] through the request extensions.
#[derive(Clone)]
struct ReqState {
    authority: Option<Vec<u8>>,
    guard: Arc<InflightReqCount>,
}

pub(crate) struct RespBody {
    kind: BodyKind,
    guard: Arc<InflightReqCount>,
    /// Declared body bytes still to pass through, with the connection state that holds the flush count.
    /// Armed by `call` once every middleware has returned.
    transport: Option<(u64, tokio::sync::watch::Receiver<bridge::ConnectionState>)>,
}

enum BodyKind {
    Reply(bridge::ReplyBody),
    Empty,
    Boxed(extension_api::Body),
}

fn refused(status: http::StatusCode, req_count: Arc<InflightReqCount>) -> http::Response<RespBody> {
    error_response(status).map(|body| RespBody {
        kind: BodyKind::Boxed(body),
        guard: req_count,
        transport: None,
    })
}

impl Body for RespBody {
    type Data = bytes::Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<bytes::Bytes>, BoxError>>> {
        let this = self.get_mut();
        let poll = match &mut this.kind {
            BodyKind::Reply(b) => Pin::new(b).poll_frame(cx),
            BodyKind::Empty => Poll::Ready(None),
            BodyKind::Boxed(b) => Pin::new(b).poll_frame(cx),
        };
        if let Some((remaining, closed)) = &mut this.transport
            && let Poll::Ready(Some(Ok(frame))) = &poll
            && let Some(data) = frame.data_ref()
        {
            *remaining = remaining.saturating_sub(data.len() as u64);
            if *remaining == 0 {
                this.guard.end_flush.get_or_init(|| closed.borrow().flushes);
            }
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            BodyKind::Reply(_) => false,
            BodyKind::Empty => true,
            BodyKind::Boxed(b) => b.is_end_stream(),
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match &self.kind {
            BodyKind::Reply(b) => b.size_hint(),
            BodyKind::Empty => http_body::SizeHint::with_exact(0),
            BodyKind::Boxed(b) => b.size_hint(),
        }
    }
}

pub(crate) struct RapiraService {
    handler: Arc<Conn>,
}

impl RapiraService {
    pub(crate) fn new(
        shared: Arc<Shared>,
        remote: Addr,
        server: Addr,
        closed: tokio::sync::watch::Receiver<bridge::ConnectionState>,
    ) -> Self {
        Self {
            handler: Arc::new(Conn {
                shared,
                closed,
                remote,
                server,
            }),
        }
    }
}

impl hyper::service::Service<http::Request<hyper::body::Incoming>> for RapiraService {
    type Response = http::Response<RespBody>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<http::Response<RespBody>, Infallible>>;

    fn call(&self, req: http::Request<hyper::body::Incoming>) -> Self::Future {
        let handler = Arc::clone(&self.handler);
        let closed = handler.closed.clone();
        let method = req.method().clone();
        Box::pin(async move {
            let mut response = handle(handler, req).await;
            // Track the body sent to hyper after all middleware has returned.
            if let Some(length) = framed_length(&method, &response) {
                let body = response.body_mut();
                if length == 0 {
                    body.guard.end_flush.get_or_init(|| closed.borrow().flushes);
                } else {
                    body.transport = Some((length, closed));
                }
            }
            Ok(response)
        })
    }
}

/// The body length hyper will frame, in the order hyper's h1 encoder (`proto/h1/role.rs`, `Server::encode`) decides it:
/// zero when the method or status forbids a body, else the content-length header, else zero for an ended stream, else the exact size hint.
/// `None` means chunked, which needs no watermark: a chunked PHP reply ends only after PHP sends End.
/// The ended-stream check also survives body combinators that drop the size hint.
fn framed_length(method: &http::Method, response: &http::Response<RespBody>) -> Option<u64> {
    let status = response.status();
    // hyper never polls the body here, whatever the headers say (`Server::can_have_body`).
    if *method == http::Method::HEAD
        || status.is_informational()
        || matches!(
            status,
            http::StatusCode::NO_CONTENT | http::StatusCode::NOT_MODIFIED
        )
    {
        return Some(0);
    }
    response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .or_else(|| response.body().is_end_stream().then_some(0))
        .or_else(|| response.body().size_hint().exact())
}

async fn handle<B>(handler: Arc<Conn>, req: http::Request<B>) -> http::Response<RespBody>
where
    B: Body<Data = bytes::Bytes> + Unpin + Send + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let reqs_counter: Arc<InflightReqCount> =
        Arc::new(InflightReqCount::init(&handler.shared.inflight));
    let received_at: f64 = std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let (mut parts, incoming) = req.into_parts();

    let authority = match check::check_request(
        &mut parts,
        handler.shared.cfg.unsafe_field_names,
        handler.shared.cfg.superglobals,
        handler.shared.cfg.max_body_size,
    ) {
        Ok(authority) => authority,
        Err(rej) => {
            tracing::warn!(target: "http", "rejected: {}", rej.reason);
            return refused(rej.status, reqs_counter);
        }
    };

    let peer: Peer = Peer {
        remote: handler.remote.clone(),
        server: handler.server.clone(),
        https: false,
        received_at,
    };

    if handler.shared.chain.is_empty() {
        return serve_php(
            &handler.shared,
            &handler.closed,
            authority,
            reqs_counter,
            &parts,
            incoming,
            peer,
        )
        .await;
    }

    parts.extensions.insert(Protocol::Http);
    parts.extensions.insert(peer);
    parts.extensions.insert(ReqState {
        authority,
        guard: Arc::clone(&reqs_counter),
    });
    let body: extension_api::Body = incoming.map_err(BoxError::from).boxed_unsync();
    let req = HttpRequest::from_parts(parts, body);

    let res = Next::new(Arc::clone(&handler.shared.chain), handler)
        .run(req)
        .await;
    // The final response and the PHP reply share one guard; the drain window
    // stays open until the last holder drops.
    res.map(|body| RespBody {
        kind: BodyKind::Boxed(body),
        guard: reqs_counter,
        transport: None,
    })
}

struct Conn {
    shared: Arc<Shared>,
    closed: tokio::sync::watch::Receiver<bridge::ConnectionState>,
    remote: Addr,
    server: Addr,
}

impl Handler for Conn {
    fn call(&self, req: HttpRequest) -> BoxFuture<'_, HttpResponse> {
        Box::pin(self.serve(req))
    }
}

impl Conn {
    async fn serve(&self, req: HttpRequest) -> HttpResponse {
        let (mut parts, body) = req.into_parts();
        let Some(state) = parts.extensions.remove::<ReqState>() else {
            tracing::error!(target: "http", "request state missing from request extensions");
            return error_response(http::StatusCode::INTERNAL_SERVER_ERROR);
        };
        let Some(peer) = parts.extensions.remove::<Peer>() else {
            tracing::error!(target: "http", "peer info missing from request extensions");
            return error_response(http::StatusCode::INTERNAL_SERVER_ERROR);
        };
        serve_php(
            &self.shared,
            &self.closed,
            state.authority,
            state.guard,
            &parts,
            body,
            peer,
        )
        .await
        .map(BodyExt::boxed_unsync)
    }
}

async fn serve_php<B>(
    shared: &Shared,
    closed: &tokio::sync::watch::Receiver<bridge::ConnectionState>,
    authority: Option<Vec<u8>>,
    guard: Arc<InflightReqCount>,
    parts: &http::request::Parts,
    body: B,
    peer: Peer,
) -> http::Response<RespBody>
where
    B: Body<Data = bytes::Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let cfg = &shared.cfg;
    let mut body = body;
    let mut collected: Vec<u8> = Vec::with_capacity(body.size_hint().lower() as usize);
    loop {
        // hyper only times the head read, so each body frame gets its own progress bound here.
        let frame = match tokio::time::timeout(cfg.keepalive_timeout, body.frame()).await {
            Ok(frame) => frame,
            Err(_) => {
                tracing::debug!(target: "http", "request body stalled past keepalive_timeout");
                return refused(http::StatusCode::REQUEST_TIMEOUT, guard);
            }
        };
        match frame {
            None => break,
            Some(Ok(frame)) => {
                // Non-data frames (request trailers) are dropped: PHP has no surface for them.
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                if collected.len() + data.len() > cfg.max_body_size {
                    tracing::warn!(target: "http", "request body exceeds max_body_size");
                    return refused(http::StatusCode::PAYLOAD_TOO_LARGE, guard);
                }
                collected.extend_from_slice(&data);
            }
            Some(Err(e)) => {
                tracing::debug!(target: "http", "request body read failed: {e}");
                return refused(http::StatusCode::BAD_REQUEST, guard);
            }
        }
    }

    let request = request::build(parts, authority, collected, peer, cfg);
    let mut reply = match shared.php.exec(request).await {
        Ok(reply) => reply,
        Err(e) => {
            if let Some(r) = e.downcast_ref::<Rejected>() {
                tracing::warn!(target: "http", "rejected before dispatch: {r}");
                let status = http::StatusCode::from_u16(r.status)
                    .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
                return refused(status, guard);
            }
            let status = if e.chain().any(|c| c.is::<std::io::Error>()) {
                http::StatusCode::INTERNAL_SERVER_ERROR
            } else {
                http::StatusCode::BAD_GATEWAY
            };
            tracing::error!(target: "http", "php exec failed: {e:#}");
            return refused(status, guard);
        }
    };

    let (status, headers, content_length, bodiless) = loop {
        match reply.next().await {
            None => {
                tracing::error!(target: "http", "php worker died before a response head");
                return refused(http::StatusCode::BAD_GATEWAY, guard);
            }
            Some(ReplyEvent::Interim { status, .. }) => {
                tracing::debug!(target: "http", "dropped interim {status}");
            }
            Some(ReplyEvent::Head {
                status,
                headers,
                content_length,
                bodiless,
                ..
            }) => break (status, headers, content_length, bodiless),
            Some(ReplyEvent::End { .. }) => {
                tracing::error!(target: "http", "php produced no response head");
                return refused(http::StatusCode::BAD_GATEWAY, guard);
            }
            Some(ReplyEvent::Chunk(_) | ReplyEvent::File { .. }) => {
                tracing::warn!(target: "http", "dropped body bytes preceding the response head");
            }
        }
    };

    let status = match http::StatusCode::from_u16(status) {
        Ok(s) if s.as_u16() >= 200 => s,
        _ => {
            // hyper reacts to a service-supplied 1xx by rewriting it to 500 and erroring
            // the connection; a 502 head keeps the connection coherent.
            tracing::error!(
                target: "http",
                "php committed status {status} as final; this front cannot forward it - serving 502"
            );
            http::StatusCode::BAD_GATEWAY
        }
    };

    let declared_cl = content_length.filter(|_| !bodiless);

    let kind = if bodiless {
        bridge::spawn_drain(reply, closed.clone(), guard.clone());
        BodyKind::Empty
    } else {
        let staged = if declared_cl.is_some() {
            tokio::time::timeout(Duration::from_millis(10), reply.next())
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        BodyKind::Reply(bridge::ReplyBody::new(
            reply,
            declared_cl,
            Arc::clone(&guard),
            staged,
            closed.clone(),
        ))
    };

    let mut res = http::Response::new(RespBody {
        kind,
        guard,
        transport: None,
    });
    *res.status_mut() = status;
    *res.headers_mut() = response_headers(headers, declared_cl);
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_api::{Backend, Reply, ReplySource, Request};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    struct NoPhp;

    impl Backend for NoPhp {
        fn exec(
            &self,
            _req: Request,
        ) -> Pin<Box<dyn Future<Output = extension_api::Result<Reply>> + Send + '_>> {
            unreachable!("the middleware answers before PHP")
        }
    }

    /// Parks a source after its scripted events until released.
    #[derive(Default)]
    struct Gate {
        released: AtomicBool,
        waker: Mutex<Option<std::task::Waker>>,
    }

    impl Gate {
        fn release(&self) {
            self.released.store(true, Ordering::Release);
            if let Some(w) = self.waker.lock().unwrap().take() {
                w.wake();
            }
        }
    }

    struct TestSource {
        events: Vec<ReplyEvent>,
        dropped: Option<Arc<AtomicBool>>,
        gate: Option<Arc<Gate>>,
    }

    impl ReplySource for TestSource {
        fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<ReplyEvent>> {
            if !self.events.is_empty() {
                return Poll::Ready(Some(self.events.remove(0)));
            }
            let Some(gate) = &self.gate else {
                return Poll::Ready(None);
            };
            // The waker is stored before the flag is read, so a release between the two still wakes.
            *gate.waker.lock().unwrap() = Some(cx.waker().clone());
            if gate.released.load(Ordering::Acquire) {
                return Poll::Ready(None);
            }
            Poll::Pending
        }
    }

    impl Drop for TestSource {
        fn drop(&mut self) {
            if let Some(flag) = &self.dropped {
                flag.store(true, Ordering::Release);
            }
        }
    }

    struct Scripted {
        scripts: Mutex<VecDeque<Vec<ReplyEvent>>>,
        seen_authorities: Mutex<Vec<Option<Vec<u8>>>>,
        dropped: Option<Arc<AtomicBool>>,
        gate: Option<Arc<Gate>>,
    }

    impl Scripted {
        fn one(events: Vec<ReplyEvent>, dropped: Option<Arc<AtomicBool>>) -> Self {
            Self {
                scripts: Mutex::new(VecDeque::from([events])),
                seen_authorities: Mutex::new(Vec::new()),
                dropped,
                gate: None,
            }
        }

        /// One script whose source parks after its events until `gate` is released.
        fn parked(events: Vec<ReplyEvent>, gate: &Arc<Gate>) -> Self {
            Self {
                scripts: Mutex::new(VecDeque::from([events])),
                seen_authorities: Mutex::new(Vec::new()),
                dropped: None,
                gate: Some(Arc::clone(gate)),
            }
        }
    }

    impl Backend for Scripted {
        fn exec(
            &self,
            req: Request,
        ) -> Pin<Box<dyn Future<Output = extension_api::Result<Reply>> + Send + '_>> {
            self.seen_authorities.lock().unwrap().push(req.authority);
            let events = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .expect("a script per exec");
            let dropped = self.dropped.clone();
            let gate = self.gate.clone();
            Box::pin(async move {
                Ok(Reply::new(Box::new(TestSource {
                    events,
                    dropped,
                    gate,
                })))
            })
        }
    }

    fn head(bodiless: bool) -> ReplyEvent {
        ReplyEvent::Head {
            status: 200,
            headers: Vec::new(),
            content_length: None,
            bodiless,
            body_coded: false,
        }
    }

    fn head_cl(content_length: u64) -> ReplyEvent {
        ReplyEvent::Head {
            status: 200,
            headers: Vec::new(),
            content_length: Some(content_length),
            bodiless: false,
            body_coded: false,
        }
    }

    fn chunk(s: &str) -> ReplyEvent {
        ReplyEvent::Chunk(bytes::Bytes::copy_from_slice(s.as_bytes()))
    }

    fn end() -> ReplyEvent {
        ReplyEvent::End {
            trailers: Vec::new(),
            truncated: false,
        }
    }

    struct Deny;

    impl Middleware for Deny {
        fn handle<'a>(&'a self, _req: HttpRequest, _next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async { error_response(http::StatusCode::FORBIDDEN) })
        }
    }

    struct Replace;

    impl Middleware for Replace {
        fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move {
                let _ = next.run(req).await;
                error_response(http::StatusCode::IM_A_TEAPOT)
            })
        }
    }

    struct Pass;

    impl Middleware for Pass {
        fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move { next.run(req).await })
        }
    }

    /// Re-boxes the body through `map_frame`, which keeps `is_end_stream` but drops the size hint.
    struct MapBody;

    impl Middleware for MapBody {
        fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move {
                next.run(req)
                    .await
                    .map(|body| body.map_frame(|frame| frame).boxed_unsync())
            })
        }
    }

    /// Adds a positive content-length to the response, as a middleware serving cached GET headers on HEAD would.
    struct HeadLength;

    impl Middleware for HeadLength {
        fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move {
                let mut res = next.run(req).await;
                res.headers_mut().insert(
                    http::header::CONTENT_LENGTH,
                    http::HeaderValue::from_static("5"),
                );
                res
            })
        }
    }

    /// Serves one request through hyper over an in-memory pipe; returns the raw response once the connection has closed.
    async fn serve_raw(
        handler: Arc<Conn>,
        closed_tx: tokio::sync::watch::Sender<bridge::ConnectionState>,
        request: &[u8],
    ) -> Vec<u8> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let io = bridge::TimedIo::new(
            hyper_util::rt::TokioIo::new(server),
            Duration::from_secs(5),
            closed_tx.clone(),
        );
        let conn = hyper::server::conn::http1::Builder::new()
            .timer(hyper_util::rt::TokioTimer::new())
            .serve_connection(io, RapiraService { handler });
        let mut closed = closed_tx.subscribe();
        tokio::spawn(async move {
            let _ = conn.await;
            closed_tx.send_modify(|s| s.closed = true);
        });
        client.write_all(request).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        closed.wait_for(|s| s.closed).await.unwrap();
        response
    }

    /// Polls on a timer, so a paused clock advances while the drain task runs.
    async fn until_inflight(inflight: &AtomicUsize, want: usize) {
        while inflight.load(Ordering::Acquire) != want {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn setup(
        backend: Arc<dyn Backend>,
        chain: Vec<Arc<dyn Middleware>>,
    ) -> (
        Arc<Conn>,
        Arc<AtomicUsize>,
        tokio::sync::watch::Sender<bridge::ConnectionState>,
    ) {
        let inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(Shared {
            cfg: Config::default(),
            php: Php::new(backend),
            chain: chain.into(),
            inflight: Arc::clone(&inflight),
        });
        let (closed_tx, closed) = tokio::sync::watch::channel(bridge::ConnectionState::default());
        let handler = Arc::new(Conn {
            shared,
            closed,
            remote: Addr::Inet(([127, 0, 0, 1], 40000).into()),
            server: Addr::Inet(([127, 0, 0, 1], 8000).into()),
        });
        (handler, inflight, closed_tx)
    }

    fn get_request() -> http::Request<http_body_util::Empty<bytes::Bytes>> {
        http::Request::builder()
            .uri("/")
            .header("host", "e2e")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap()
    }

    /// A middleware answer must hold the inflight guard until hyper drops the body.
    #[tokio::test]
    async fn short_circuit_keeps_the_inflight_guard() {
        let (handler, inflight, _closed_tx) =
            setup(Arc::new(NoPhp), vec![Arc::new(Deny) as Arc<dyn Middleware>]);
        let res = handle(handler, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::FORBIDDEN);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the guard must ride the response body"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }

    /// A middleware that replaces the PHP response must keep the request counted
    /// until hyper drops the replacement body.
    #[tokio::test]
    async fn replaced_response_keeps_the_inflight_guard() {
        let backend = Arc::new(Scripted::one(vec![head(false), end()], None));
        let (handler, inflight, _closed_tx) =
            setup(backend, vec![Arc::new(Replace) as Arc<dyn Middleware>]);
        let res = handle(handler, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::IM_A_TEAPOT);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the guard must ride the replacement response"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }

    /// One request counts once, no matter how many holders share the guard.
    #[tokio::test]
    async fn chained_response_counts_one_request() {
        let backend = Arc::new(Scripted::one(vec![head(false), end()], None));
        let (handler, inflight, _closed_tx) =
            setup(backend, vec![Arc::new(Pass) as Arc<dyn Middleware>]);
        let res = handle(handler, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::OK);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "one request must count once"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }

    /// A bodiless reply keeps the response guarded after its reply is consumed to End.
    #[tokio::test]
    async fn bodiless_response_stays_guarded_after_the_reply_ends() {
        let dropped = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(Scripted::one(
            vec![head(true), end()],
            Some(Arc::clone(&dropped)),
        ));
        let (handler, inflight, _closed_tx) = setup(backend, Vec::new());
        let res = handle(handler, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reply must be consumed to End");
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the guard must ride the empty response"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }

    /// One handler serves the whole connection; each request must carry its own state.
    #[tokio::test]
    async fn sequential_requests_share_the_handler_but_not_the_state() {
        let backend = Arc::new(Scripted {
            scripts: Mutex::new(VecDeque::from([
                vec![head(false), end()],
                vec![head(false), end()],
            ])),
            seen_authorities: Mutex::new(Vec::new()),
            dropped: None,
            gate: None,
        });
        let (handler, inflight, _closed_tx) = setup(
            Arc::clone(&backend) as Arc<dyn Backend>,
            vec![Arc::new(Pass) as Arc<dyn Middleware>],
        );
        for _ in 0..2 {
            let res = handle(Arc::clone(&handler), get_request()).await;
            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(inflight.load(Ordering::Acquire), 1);
            drop(res);
            assert_eq!(inflight.load(Ordering::Acquire), 0);
        }
        assert_eq!(
            *backend.seen_authorities.lock().unwrap(),
            vec![Some(b"e2e".to_vec()), Some(b"e2e".to_vec())],
            "every exec must see the authority of its own request"
        );
    }

    /// The chain path must hand the guard to the drain task: the request stays
    /// counted after the response is gone, until the reply stream ends.
    #[tokio::test]
    async fn a_parked_drain_keeps_the_request_counted_through_the_chain() {
        let gate = Arc::new(Gate::default());
        let backend = Arc::new(Scripted::parked(vec![head(true)], &gate));
        let (handler, inflight, _closed_tx) =
            setup(backend, vec![Arc::new(Pass) as Arc<dyn Middleware>]);
        let res = handle(handler, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::OK);
        drop(res);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the drain task must keep the request counted"
        );
        gate.release();
        tokio::time::timeout(Duration::from_secs(5), until_inflight(&inflight, 0))
            .await
            .expect("drain must release the count at the stream end");
    }

    /// `map_frame` drops the size hint, so the bodiless watermark must come from `is_end_stream`.
    #[tokio::test(start_paused = true)]
    async fn delivered_head_behind_body_mapping_middleware_keeps_php_alive() {
        let gate = Arc::new(Gate::default());
        let backend = Arc::new(Scripted::parked(vec![head(true)], &gate));
        let (handler, inflight, closed_tx) =
            setup(backend, vec![Arc::new(MapBody) as Arc<dyn Middleware>]);
        let response = serve_raw(
            handler,
            closed_tx,
            b"HEAD / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        // The paused clock auto-advances once every task is idle, so the timeout proves the drain kept the reply.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), until_inflight(&inflight, 0))
                .await
                .is_err(),
            "a close after the flushed head must not cancel PHP"
        );
        gate.release();
        tokio::time::timeout(Duration::from_secs(5), until_inflight(&inflight, 0))
            .await
            .expect("End releases the count");
    }

    /// hyper writes no body for HEAD whatever content-length says, so the head alone completes the response.
    #[tokio::test(start_paused = true)]
    async fn head_with_a_positive_content_length_completes_at_the_head() {
        let gate = Arc::new(Gate::default());
        let backend = Arc::new(Scripted::parked(vec![head(true)], &gate));
        let (handler, inflight, closed_tx) =
            setup(backend, vec![Arc::new(HeadLength) as Arc<dyn Middleware>]);
        let response = serve_raw(
            handler,
            closed_tx,
            b"HEAD / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            response.starts_with(b"HTTP/1.1 200") && response.ends_with(b"\r\n\r\n"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), until_inflight(&inflight, 0))
                .await
                .is_err(),
            "a close after the flushed head must not cancel PHP"
        );
        gate.release();
        tokio::time::timeout(Duration::from_secs(5), until_inflight(&inflight, 0))
            .await
            .expect("End releases the count");
    }

    /// hyper drops a length-delimited body once the last byte is buffered; the flush after it must keep PHP alive past the close.
    #[tokio::test(start_paused = true)]
    async fn delivered_fixed_length_body_keeps_php_alive_past_the_close() {
        let gate = Arc::new(Gate::default());
        let backend = Arc::new(Scripted::parked(vec![head_cl(5), chunk("01234")], &gate));
        let (handler, inflight, closed_tx) = setup(backend, Vec::new());
        let response = serve_raw(
            handler,
            closed_tx,
            b"GET / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        )
        .await;
        let text = String::from_utf8_lossy(&response).to_ascii_lowercase();
        assert!(
            text.contains("content-length: 5") && text.ends_with("\r\n\r\n01234"),
            "{text}"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), until_inflight(&inflight, 0))
                .await
                .is_err(),
            "a close after the flushed body must not cancel PHP"
        );
        gate.release();
        tokio::time::timeout(Duration::from_secs(5), until_inflight(&inflight, 0))
            .await
            .expect("End releases the count");
    }
}
