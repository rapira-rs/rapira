use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use http::header::CONTENT_TYPE;
use http_body::Body;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use rapira_sapi::work::{Intake, Refused};
use rapira_sapi::{Addr, Frame, Request};
use tower::{Service as _, ServiceExt as _};

use crate::check::{self, Rejection};
use crate::middleware::{self, BoxError, Layer, Peer, Service};
use crate::response::{error_response, response_headers};
use crate::{Config, Exchange, bridge, multipart, request};

pub(crate) struct Shared {
    pub cfg: Config,
    pub intake: Intake<Exchange>,
    /// The multipart limits; None outside dispatcher mode.
    pub uploads: Option<Arc<multipart::Limits>>,
    pub inflight: Arc<AtomicUsize>,
}

impl From<Refused> for Rejection {
    fn from(e: Refused) -> Self {
        Self {
            status: match e {
                Refused::Saturated => http::StatusCode::SERVICE_UNAVAILABLE,
                Refused::Stopped => http::StatusCode::INTERNAL_SERVER_ERROR,
            },
            reason: e.to_string(),
        }
    }
}

impl From<multipart::ParseError> for Rejection {
    fn from(e: multipart::ParseError) -> Self {
        match e {
            multipart::ParseError::Rejected { status, reason } => Self { status, reason },
            multipart::ParseError::Io(e) => Self {
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                reason: format!("upload spool failed: {e}"),
            },
        }
    }
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
    /// Armed by [`respond`] once every middleware has returned.
    transport: Option<(u64, tokio::sync::watch::Receiver<bridge::ConnectionState>)>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "one value per response; a Box would cost an allocation per response"
)]
enum BodyKind {
    Reply(bridge::ReplyBody),
    Empty,
    Boxed(middleware::Body),
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

/// Serves one request of the connection. `chain` is [`Conn::chain`].
pub(crate) async fn respond(
    handler: Arc<Conn>,
    chain: Option<Service>,
    req: http::Request<Incoming>,
) -> http::Response<RespBody> {
    let closed = handler.closed.clone();
    let method = req.method().clone();
    let mut response = handle(handler, chain, req).await;
    // Track the body sent to hyper after all middleware has returned.
    if let Some(length) = framed_length(&method, &response) {
        let body = response.body_mut();
        if length == 0 {
            body.guard.end_flush.get_or_init(|| closed.borrow().flushes);
        } else {
            body.transport = Some((length, closed));
        }
    }
    response
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

async fn handle<B>(
    handler: Arc<Conn>,
    chain: Option<Service>,
    req: http::Request<B>,
) -> http::Response<RespBody>
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

    let Some(chain) = chain else {
        return serve_php(
            &handler.shared,
            &handler.closed,
            authority,
            reqs_counter,
            &mut parts,
            incoming,
            peer,
        )
        .await;
    };

    parts.extensions.insert(peer);
    parts.extensions.insert(ReqState {
        authority,
        guard: Arc::clone(&reqs_counter),
    });
    let body: middleware::Body = incoming.map_err(BoxError::from).boxed_unsync();
    let req = middleware::Request::from_parts(parts, body);

    // The awaited future is the boxed one that `call` returns: the compiler cannot prove `Send` for a held `Oneshot` over this request type.
    // https://github.com/rust-lang/rust/issues/110338
    let mut chain = chain;
    let Ok(ready) = chain.ready().await;
    let Ok(res) = ready.call(req).await;
    // The final response and the PHP reply share one guard; the drain window
    // stays open until the last holder drops.
    res.map(|body| RespBody {
        kind: BodyKind::Boxed(body),
        guard: reqs_counter,
        transport: None,
    })
}

/// Wraps `inner` in `layers`, the first listed outermost.
fn fold(layers: &[Layer], inner: Service) -> Service {
    layers
        .iter()
        .rev()
        .fold(inner, |inner, layer| tower::Layer::layer(layer, inner))
}

/// One connection. [`Conn::serve`] is the inner service of its middleware chain.
pub(crate) struct Conn {
    shared: Arc<Shared>,
    closed: tokio::sync::watch::Receiver<bridge::ConnectionState>,
    remote: Addr,
    server: Addr,
}

impl Conn {
    pub(crate) fn new(
        shared: Arc<Shared>,
        remote: Addr,
        server: Addr,
        closed: tokio::sync::watch::Receiver<bridge::ConnectionState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            shared,
            closed,
            remote,
            server,
        })
    }

    /// The configured middleware around [`Conn::serve`]. None without middleware.
    pub(crate) fn chain(self: &Arc<Self>) -> Option<Service> {
        let layers = &self.shared.cfg.middleware;
        if layers.is_empty() {
            return None;
        }
        let conn = Arc::clone(self);
        let serve = tower::service_fn(move |req| {
            let conn = Arc::clone(&conn);
            async move { Ok::<_, Infallible>(conn.serve(req).await) }
        });
        Some(fold(layers, Service::new(serve)))
    }

    async fn serve(&self, req: middleware::Request) -> middleware::Response {
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
            &mut parts,
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
    parts: &mut http::request::Parts,
    body: B,
    peer: Peer,
) -> http::Response<RespBody>
where
    B: Body<Data = bytes::Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let cfg = &shared.cfg;
    let mut body = body;
    // The direct path bounds the hint through the content-length check; a middleware body can report any lower bound.
    let reserve = body.size_hint().lower().min(cfg.max_body_size as u64) as usize;
    let mut collected: Vec<u8> = Vec::with_capacity(reserve);
    loop {
        // hyper only times the head read, so each body frame gets its own progress bound here.
        let frame = match timeout_lazy(cfg.keepalive_timeout, body.frame()).await {
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
    let mut reply = match submit(shared, request).await {
        Ok(reply) => reply,
        Err(r) => {
            tracing::warn!(target: "http", "rejected before dispatch: {r}");
            return refused(r.status, guard);
        }
    };

    let (status, headers, content_length, bodiless) = loop {
        match reply.recv().await {
            None => {
                tracing::error!(target: "http", "php worker died before a response head");
                return refused(http::StatusCode::BAD_GATEWAY, guard);
            }
            Some(Frame::Interim(head)) => {
                tracing::debug!(target: "http", "dropped interim {}", head.status);
            }
            Some(Frame::Head {
                head,
                content_length,
                bodiless,
            }) => break (head.status, head.headers, content_length, bodiless),
            Some(Frame::End { .. }) => {
                tracing::error!(target: "http", "php produced no response head");
                return refused(http::StatusCode::BAD_GATEWAY, guard);
            }
            Some(Frame::Chunk(_) | Frame::File { .. }) => {
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
                "php committed status {status} as final; this plugin cannot forward it - serving 502"
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
            timeout_lazy(Duration::from_millis(10), reply.recv())
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

/// Polls `fut` once and arms the timer only when it is pending: a ready future needs no timer.
async fn timeout_lazy<F: Future>(
    dur: Duration,
    fut: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    let mut fut = std::pin::pin!(fut);
    match std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx))).await {
        Poll::Ready(out) => Ok(out),
        Poll::Pending => tokio::time::timeout(dur, fut).await,
    }
}

/// Both refusals come before dispatch: the multipart parse, then the intake.
async fn submit(
    shared: &Shared,
    request: Request,
) -> Result<tokio::sync::mpsc::Receiver<Frame>, Rejection> {
    let request = parse_multipart(request, shared.uploads.as_ref()).await?;
    let (exchange, reply) = Exchange::new(request, shared.cfg.superglobals);
    shared.intake.submit(exchange).await?;
    Ok(reply)
}

/// Parses a multipart body before submit, so a rejected body never reaches the pending and active counters. `limits` is None outside dispatcher mode.
async fn parse_multipart(
    mut req: Request,
    limits: Option<&Arc<multipart::Limits>>,
) -> Result<Request, Rejection> {
    let Some(limits) = limits else {
        return Ok(req);
    };
    let rapira_sapi::types::Body::Raw(raw) = &mut req.body else {
        return Ok(req);
    };
    if raw.get_ref().is_empty() {
        return Ok(req);
    }
    // Content-type is a singleton field per RFC 9110 §8.3: with repeated lines the plugin and a PHP consumer could split the body on different boundaries.
    // https://www.rfc-editor.org/rfc/rfc9110#section-8.3
    let lines = req.headers.get_all(CONTENT_TYPE);
    if lines.iter().nth(1).is_some() && lines.iter().any(|v| multipart::is_multipart(v.as_bytes()))
    {
        return Err(Rejection {
            status: http::StatusCode::BAD_REQUEST,
            reason: "repeated content-type field lines with a multipart body".into(),
        });
    }
    let Some(content_type) = req.content_type.as_deref() else {
        return Ok(req);
    };
    if !multipart::is_multipart(content_type) {
        return Ok(req);
    }
    let boundary = multipart::boundary(content_type)?;
    let bytes = std::mem::take(raw.get_mut());
    let limits = Arc::clone(limits);
    let parsed = tokio::task::spawn_blocking(move || multipart::parse(&bytes, &boundary, &limits))
        .await
        .map_err(|e| Rejection {
            status: http::StatusCode::INTERNAL_SERVER_ERROR,
            reason: format!("multipart parse task failed: {e}"),
        })?;
    req.body = rapira_sapi::types::Body::Multipart(parsed?);
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_sapi::ResponseHead;
    use rapira_sapi::types::Body as SapiBody;
    use rapira_sapi::work::Work as _;
    use tokio::sync::mpsc;
    use tower::layer::layer_fn;
    use tower::service_fn;
    use tower::util::MapResponseLayer;

    use crate::response::empty_body;

    /// Takes the request and the reply sender of `exchange` as the PHP thread does in the classic and worker modes.
    fn take(exchange: Exchange) -> (Request, mpsc::Sender<Frame>) {
        let ctx = Box::new(exchange)
            .into_cgi()
            .expect("an exchange has a CGI context");
        let tx = ctx.sender.expect("a queued exchange holds its sender");
        (ctx.req, tx)
    }

    /// An intake that nothing drains: the middleware answers before PHP.
    fn no_php() -> Intake<Exchange> {
        Intake::channel(1).0
    }

    /// Plays PHP for one exchange: pulls it and sends `frames`. The task returns the frame sender, which keeps the reply open.
    fn php_one(
        frames: Vec<Frame>,
    ) -> (
        Intake<Exchange>,
        tokio::task::JoinHandle<mpsc::Sender<Frame>>,
    ) {
        let (intake, mut units) = Intake::<Exchange>::channel(1);
        let php = tokio::spawn(async move {
            let (_, tx) = take(units.recv().await.expect("an exchange"));
            for frame in frames {
                tx.send(frame).await.unwrap();
            }
            tx
        });
        (intake, php)
    }

    fn ok_head() -> ResponseHead {
        ResponseHead {
            status: 200,
            headers: http::HeaderMap::new(),
        }
    }

    fn head(bodiless: bool) -> Frame {
        Frame::Head {
            head: ok_head(),
            content_length: None,
            bodiless,
        }
    }

    fn head_cl(content_length: u64) -> Frame {
        Frame::Head {
            head: ok_head(),
            content_length: Some(content_length),
            bodiless: false,
        }
    }

    fn chunk(s: &str) -> Frame {
        Frame::Chunk(bytes::Bytes::copy_from_slice(s.as_bytes()))
    }

    fn end() -> Frame {
        Frame::End {
            trailers: http::HeaderMap::new(),
            truncated: false,
        }
    }

    fn deny() -> Layer {
        Layer::new(layer_fn(|_inner: Service| {
            service_fn(|_req: middleware::Request| async {
                Ok(error_response(http::StatusCode::FORBIDDEN))
            })
        }))
    }

    fn replace() -> Layer {
        Layer::new(MapResponseLayer::new(|_res: middleware::Response| {
            error_response(http::StatusCode::IM_A_TEAPOT)
        }))
    }

    fn pass() -> Layer {
        Layer::new(tower::layer::util::Identity::new())
    }

    /// Re-boxes the body through `map_frame`, which keeps `is_end_stream` but drops the size hint.
    fn map_body() -> Layer {
        Layer::new(MapResponseLayer::new(|res: middleware::Response| {
            res.map(|body| body.map_frame(|frame| frame).boxed_unsync())
        }))
    }

    /// Adds a positive content-length to the response, as a middleware serving cached GET headers on HEAD would.
    fn head_length() -> Layer {
        Layer::new(MapResponseLayer::new(|mut res: middleware::Response| {
            res.headers_mut().insert(
                http::header::CONTENT_LENGTH,
                http::HeaderValue::from_static("5"),
            );
            res
        }))
    }

    /// Appends `{name}-in` to the request and `{name}-out` to the response.
    fn tag(name: &'static str) -> Layer {
        Layer::new(
            tower::ServiceBuilder::new()
                .map_request(move |mut req: middleware::Request| {
                    req.headers_mut()
                        .append("x-trace", format!("{name}-in").parse().unwrap());
                    req
                })
                .map_response(move |mut res: middleware::Response| {
                    res.headers_mut()
                        .append("x-trace", format!("{name}-out").parse().unwrap());
                    res
                }),
        )
    }

    /// Answers 200 with the `x-trace` values of the request.
    fn echo() -> Service {
        Service::new(service_fn(|req: middleware::Request| async move {
            let mut res = http::Response::builder()
                .status(200)
                .body(empty_body())
                .unwrap();
            for v in req.headers().get_all("x-trace") {
                res.headers_mut().append("x-trace", v.clone());
            }
            Ok(res)
        }))
    }

    fn trace(res: &middleware::Response) -> Vec<&str> {
        res.headers()
            .get_all("x-trace")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chain_runs_outermost_first_and_unwinds_in_reverse() {
        let chain = fold(&[tag("a"), tag("b")], echo());
        let Ok(res) = chain.oneshot(http::Request::new(empty_body())).await;
        assert_eq!(res.status(), 200);
        assert_eq!(trace(&res), ["a-in", "b-in", "b-out", "a-out"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn short_circuit_skips_downstream_and_the_handler() {
        let chain = fold(&[tag("a"), deny(), tag("never")], echo());
        let Ok(res) = chain.oneshot(http::Request::new(empty_body())).await;
        assert_eq!(res.status(), 403);
        assert_eq!(trace(&res), ["a-out"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_chain_reaches_the_handler_directly() {
        let chain = fold(&[], echo());
        let mut req = http::Request::new(empty_body());
        req.headers_mut().append("x-trace", "solo".parse().unwrap());
        let Ok(res) = chain.oneshot(req).await;
        assert_eq!(res.status(), 200);
        assert_eq!(trace(&res), ["solo"]);
    }

    /// Serves `req` as the connection of `handler` would, without hyper.
    async fn call<B>(handler: &Arc<Conn>, req: http::Request<B>) -> http::Response<RespBody>
    where
        B: Body<Data = bytes::Bytes> + Unpin + Send + 'static,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        handle(Arc::clone(handler), handler.chain(), req).await
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
        let mut builder = hyper::server::conn::http1::Builder::new();
        builder.timer(hyper_util::rt::TokioTimer::new());
        // Dropping the shutdown handle would start a graceful shutdown, so it lives until the connection closes.
        let graceful = hyper_util::server::graceful::GracefulShutdown::new();
        let mut closed = closed_tx.subscribe();
        crate::serve::spawn_connection(&builder, &graceful, io, handler, closed_tx);
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
        intake: Intake<Exchange>,
        uploads: Option<Arc<multipart::Limits>>,
        middleware: Vec<Layer>,
    ) -> (
        Arc<Conn>,
        Arc<AtomicUsize>,
        tokio::sync::watch::Sender<bridge::ConnectionState>,
    ) {
        let inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(Shared {
            cfg: Config {
                middleware,
                ..Config::default()
            },
            intake,
            uploads,
            inflight: Arc::clone(&inflight),
        });
        let (closed_tx, closed) = tokio::sync::watch::channel(bridge::ConnectionState::default());
        let handler = Conn::new(
            shared,
            Addr::Inet(([127, 0, 0, 1], 40000).into()),
            Addr::Inet(([127, 0, 0, 1], 8000).into()),
            closed,
        );
        (handler, inflight, closed_tx)
    }

    fn get_request() -> http::Request<http_body_util::Empty<bytes::Bytes>> {
        http::Request::builder()
            .uri("/")
            .header("host", "e2e")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap()
    }

    /// A malformed or over-limit multipart body answers before dispatch and never reaches PHP; an accepted body reaches PHP raw. Sources: RFC 7578 (multipart/form-data needs a boundary), RFC 9110 §8.3 (content-type is a singleton field).
    #[tokio::test]
    async fn rejected_bodies_never_reach_php() {
        struct Case {
            name: &'static str,
            method: &'static str,
            content_type: &'static [&'static str],
            body: Vec<u8>,
            /// None: the request reaches PHP with `body` unparsed.
            status: Option<u16>,
        }
        const MULTIPART: &str = "multipart/form-data; boundary=B";
        const EVIL: &str = "multipart/form-data; boundary=EVIL";
        let smuggled =
            b"--EVIL\r\ncontent-disposition: form-data; name=a\r\n\r\n1\r\n--EVIL--".to_vec();
        let cases = [
            Case {
                name: "multipart without a boundary line",
                method: "POST",
                content_type: &[MULTIPART],
                body: b"no boundary here".to_vec(),
                status: Some(400),
            },
            Case {
                name: "file part over max_file_size",
                method: "POST",
                content_type: &[MULTIPART],
                body: [
                    &b"--B\r\ncontent-disposition: form-data; name=f; filename=a\r\n\r\n"[..],
                    &[b'x'; 8192],
                    b"\r\n--B--",
                ]
                .concat(),
                status: Some(413),
            },
            Case {
                name: "plain line before a repeated multipart line",
                method: "POST",
                content_type: &["text/plain", EVIL],
                body: smuggled.clone(),
                status: Some(400),
            },
            Case {
                name: "multipart line before a repeated plain line",
                method: "POST",
                content_type: &[EVIL, "text/plain"],
                body: smuggled,
                status: Some(400),
            },
            Case {
                name: "empty multipart body",
                method: "POST",
                content_type: &[MULTIPART],
                body: Vec::new(),
                status: None,
            },
            Case {
                name: "plain body that looks like multipart",
                method: "POST",
                content_type: &["text/plain"],
                body: b"--B\r\nnot really\r\n--B--".to_vec(),
                status: None,
            },
            Case {
                name: "get",
                method: "GET",
                content_type: &[],
                body: Vec::new(),
                status: None,
            },
        ];

        let spool = tempfile::tempdir().unwrap();
        let limits = multipart::Limits {
            dir: spool.path().to_path_buf(),
            max_file_size: 1024,
            ..multipart::Limits::default()
        };
        let (intake, mut units) = Intake::<Exchange>::channel(1);
        let (handler, _inflight, _closed_tx) = setup(intake, Some(Arc::new(limits)), Vec::new());
        for case in cases {
            let mut request = http::Request::builder()
                .method(case.method)
                .uri("/")
                .header("host", "e2e");
            for line in case.content_type {
                request = request.header(http::header::CONTENT_TYPE, *line);
            }
            let request = request
                .body(http_body_util::Full::new(bytes::Bytes::from(
                    case.body.clone(),
                )))
                .unwrap();
            let status = match case.status {
                Some(_) => call(&handler, request).await.status(),
                None => {
                    let (res, body) = tokio::join!(call(&handler, request), async {
                        let (req, tx) = take(units.recv().await.expect("an exchange"));
                        let SapiBody::Raw(body) = req.body else {
                            panic!("{}: the body was parsed", case.name);
                        };
                        let body = body.into_inner();
                        tx.send(head(false)).await.unwrap();
                        tx.send(end()).await.unwrap();
                        body
                    });
                    assert_eq!(body, case.body, "{}", case.name);
                    res.status()
                }
            };
            assert_eq!(status.as_u16(), case.status.unwrap_or(200), "{}", case.name);
            assert!(
                units.try_recv().is_err(),
                "{}: a unit reached PHP",
                case.name
            );
        }
    }

    /// A middleware answer must hold the inflight guard until hyper drops the body.
    #[tokio::test]
    async fn short_circuit_keeps_the_inflight_guard() {
        let (handler, inflight, _closed_tx) = setup(no_php(), None, vec![deny()]);
        let res = call(&handler, get_request()).await;
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
        let (intake, _php) = php_one(vec![head(false), end()]);
        let (handler, inflight, _closed_tx) = setup(intake, None, vec![replace()]);
        let res = call(&handler, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::IM_A_TEAPOT);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the guard must ride the replacement response"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }

    /// A bodiless reply keeps the response guarded after its reply is consumed to End.
    #[tokio::test]
    async fn bodiless_response_stays_guarded_after_the_reply_ends() {
        let (intake, php) = php_one(vec![head(true), end()]);
        let (handler, inflight, _closed_tx) = setup(intake, None, Vec::new());
        let res = call(&handler, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::OK);
        let events = php.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), events.closed())
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
        let (intake, mut units) = Intake::<Exchange>::channel(1);
        let php = tokio::spawn(async move {
            let mut authorities = Vec::new();
            while let Some(exchange) = units.recv().await {
                let (req, tx) = take(exchange);
                authorities.push(req.authority);
                tx.send(head(false)).await.unwrap();
                tx.send(end()).await.unwrap();
            }
            authorities
        });
        let (handler, inflight, _closed_tx) = setup(intake, None, vec![pass()]);
        for _ in 0..2 {
            let res = call(&handler, get_request()).await;
            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(inflight.load(Ordering::Acquire), 1);
            drop(res);
            assert_eq!(inflight.load(Ordering::Acquire), 0);
        }
        drop(handler);
        assert_eq!(
            php.await.unwrap(),
            vec![Some(b"e2e".to_vec()), Some(b"e2e".to_vec())],
            "every exchange must carry the authority of its own request"
        );
    }

    /// The chain path must hand the guard to the drain task: the request stays
    /// counted after the response is gone, until the reply stream ends.
    #[tokio::test]
    async fn a_parked_drain_keeps_the_request_counted_through_the_chain() {
        let (intake, php) = php_one(vec![head(true)]);
        let (handler, inflight, _closed_tx) = setup(intake, None, vec![pass()]);
        let res = call(&handler, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::OK);
        let events = php.await.unwrap();
        drop(res);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the drain task must keep the request counted"
        );
        events.send(end()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), until_inflight(&inflight, 0))
            .await
            .expect("drain must release the count at the stream end");
    }

    /// `map_frame` drops the size hint, so the bodiless watermark must come from `is_end_stream`.
    #[tokio::test(start_paused = true)]
    async fn delivered_head_behind_body_mapping_middleware_keeps_php_alive() {
        let (intake, php) = php_one(vec![head(true)]);
        let (handler, inflight, closed_tx) = setup(intake, None, vec![map_body()]);
        let response = serve_raw(
            handler,
            closed_tx,
            b"HEAD / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        )
        .await;
        let events = php.await.unwrap();
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
        events.send(end()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), until_inflight(&inflight, 0))
            .await
            .expect("End releases the count");
    }

    /// hyper writes no body for HEAD whatever content-length says, so the head alone completes the response.
    #[tokio::test(start_paused = true)]
    async fn head_with_a_positive_content_length_completes_at_the_head() {
        let (intake, php) = php_one(vec![head(true)]);
        let (handler, inflight, closed_tx) = setup(intake, None, vec![head_length()]);
        let response = serve_raw(
            handler,
            closed_tx,
            b"HEAD / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        )
        .await;
        let events = php.await.unwrap();
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
        events.send(end()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), until_inflight(&inflight, 0))
            .await
            .expect("End releases the count");
    }

    /// hyper drops a length-delimited body once the last byte is buffered; the flush after it must keep PHP alive past the close.
    #[tokio::test(start_paused = true)]
    async fn delivered_fixed_length_body_keeps_php_alive_past_the_close() {
        let (intake, php) = php_one(vec![head_cl(5), chunk("01234")]);
        let (handler, inflight, closed_tx) = setup(intake, None, Vec::new());
        let response = serve_raw(
            handler,
            closed_tx,
            b"GET / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        )
        .await;
        let events = php.await.unwrap();
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
        events.send(end()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), until_inflight(&inflight, 0))
            .await
            .expect("End releases the count");
    }

    /// Pending for `left` polls, then ready. Each pending poll wakes the task at once.
    struct ReadyAfter {
        left: Option<u32>,
    }

    impl Future for ReadyAfter {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            match self.left {
                Some(0) => Poll::Ready(()),
                Some(n) => {
                    self.left = Some(n - 1);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                None => Poll::Pending,
            }
        }
    }

    /// Outside a runtime `tokio::time::timeout` panics: a ready future must complete without it.
    #[test]
    fn timeout_lazy_ready_future_needs_no_timer() {
        let mut fut = std::pin::pin!(timeout_lazy(
            Duration::from_secs(1),
            ReadyAfter { left: Some(0) }
        ));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timeout_lazy_outcomes() {
        struct Case {
            name: &'static str,
            pending_polls: Option<u32>,
            elapsed: bool,
            waited: Duration,
        }
        let dur = Duration::from_millis(10);
        let cases = [
            Case {
                name: "ready future returns at once",
                pending_polls: Some(0),
                elapsed: false,
                waited: Duration::ZERO,
            },
            Case {
                name: "future ready after the first poll returns Ok",
                pending_polls: Some(1),
                elapsed: false,
                waited: Duration::ZERO,
            },
            Case {
                name: "future ready after several polls returns Ok",
                pending_polls: Some(3),
                elapsed: false,
                waited: Duration::ZERO,
            },
            Case {
                name: "pending future times out at the deadline",
                pending_polls: None,
                elapsed: true,
                waited: dur,
            },
        ];
        for case in cases {
            let start = tokio::time::Instant::now();
            let out = timeout_lazy(
                dur,
                ReadyAfter {
                    left: case.pending_polls,
                },
            )
            .await;
            assert_eq!(out.is_err(), case.elapsed, "{}", case.name);
            assert_eq!(start.elapsed(), case.waited, "{}", case.name);
        }
    }
}
