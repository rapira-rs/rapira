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
        Box::pin(async move {
            let mut response = handle(handler, req).await;
            let length = if response
                .headers()
                .contains_key(http::header::TRANSFER_ENCODING)
            {
                None
            } else {
                response
                    .headers()
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .or_else(|| response.body().size_hint().exact())
            };
            // Track the body sent to hyper after all middleware has returned.
            if let Some(length) = length {
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
            &peer,
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
            &peer,
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
    peer: &Peer,
) -> http::Response<RespBody>
where
    B: Body<Data = bytes::Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let cfg = &shared.cfg;
    let mut body = body;
    let mut collected: Vec<u8> = Vec::new();
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

    struct TestSource {
        events: Vec<ReplyEvent>,
        dropped: Option<Arc<AtomicBool>>,
    }

    impl ReplySource for TestSource {
        fn poll_next(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ReplyEvent>> {
            match self.events.is_empty() {
                true => Poll::Ready(None),
                false => Poll::Ready(Some(self.events.remove(0))),
            }
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
    }

    impl Scripted {
        fn one(events: Vec<ReplyEvent>, dropped: Option<Arc<AtomicBool>>) -> Self {
            Self {
                scripts: Mutex::new(VecDeque::from([events])),
                seen_authorities: Mutex::new(Vec::new()),
                dropped,
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
            Box::pin(async move { Ok(Reply::new(Box::new(TestSource { events, dropped }))) })
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

    /// Sends a bodiless head, then parks until released, so the drain outlives the response.
    struct ParkedSource {
        events: Vec<ReplyEvent>,
        released: Arc<AtomicBool>,
    }

    impl ReplySource for ParkedSource {
        fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<ReplyEvent>> {
            if !self.events.is_empty() {
                return Poll::Ready(Some(self.events.remove(0)));
            }
            if self.released.load(Ordering::Acquire) {
                return Poll::Ready(None);
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    struct Parked {
        released: Arc<AtomicBool>,
    }

    impl Backend for Parked {
        fn exec(
            &self,
            _req: Request,
        ) -> Pin<Box<dyn Future<Output = extension_api::Result<Reply>> + Send + '_>> {
            let source = ParkedSource {
                events: vec![head(true)],
                released: Arc::clone(&self.released),
            };
            Box::pin(async move { Ok(Reply::new(Box::new(source))) })
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

    /// A bodiless reply keeps the response guarded after the drain task finishes.
    #[tokio::test]
    async fn bodiless_response_stays_guarded_after_the_drain_ends() {
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
        .expect("drain must run to End");
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
        let released = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(Parked {
            released: Arc::clone(&released),
        });
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
        released.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(5), async {
            while inflight.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drain must release the count at the stream end");
    }
}
