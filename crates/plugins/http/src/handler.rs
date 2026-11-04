//! One request: the admission checks, the middleware chain, the body read, the submit to the intake and the response head.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use http::header::CONTENT_TYPE;
use http_body_util::BodyExt;
use hyper::body::{Body, Bytes, Incoming, SizeHint};
use rapira_sapi::types::ResponseHead;
use rapira_sapi::work::{Intake, Refused, Unit};
use rapira_sapi::{Addr, Frame, Request};
use tower::{Service as _, ServiceExt as _};

use crate::check::{self, Rejection};
use crate::middleware::{self, BoxError, Layer, Peer, Service};
use crate::response::{error_response, response_headers};
use crate::{Config, Exchange, bridge, multipart, request};

pub(crate) struct Shared {
    pub cfg: Config,
    pub intake: Intake,
    pub inflight: Arc<AtomicUsize>,
    /// The worker drains: every response closes its connection, so the client sends its next request to another worker.
    pub closing: AtomicBool,
}

impl From<Refused> for Rejection {
    fn from(e: Refused) -> Self {
        Self {
            status: match e {
                Refused::Saturated | Refused::OverBudget => http::StatusCode::SERVICE_UNAVAILABLE,
                Refused::Stopped => http::StatusCode::INTERNAL_SERVER_ERROR,
            },
            reason: e.to_string(),
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
    peer: Peer,
}

pub(crate) struct RespBody {
    kind: BodyKind,
    guard: Arc<InflightReqCount>,
    /// Armed by [`respond`] once every middleware has returned.
    transport: Option<Declared>,
}

struct Declared {
    /// Declared body bytes still to pass through.
    remaining: u64,
    /// The connection state that holds the flush count.
    closed: tokio::sync::watch::Receiver<bridge::ConnectionState>,
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
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let poll = match &mut this.kind {
            BodyKind::Reply(b) => Pin::new(b).poll_frame(cx),
            BodyKind::Empty => Poll::Ready(None),
            BodyKind::Boxed(b) => Pin::new(b).poll_frame(cx),
        };
        if let Some(Declared { remaining, closed }) = &mut this.transport
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

    fn size_hint(&self) -> SizeHint {
        match &self.kind {
            BodyKind::Reply(b) => b.size_hint(),
            BodyKind::Empty => SizeHint::with_exact(0),
            BodyKind::Boxed(b) => b.size_hint(),
        }
    }
}

/// Serves one request of the connection. `chain` is [`Conn::chain`].
pub(crate) async fn respond(
    handler: &Conn,
    chain: Option<Service>,
    req: http::Request<Incoming>,
) -> http::Response<RespBody> {
    let method = req.method().clone();
    let mut response = handle(handler, chain, req).await;
    // Read after the request: one that ends during the drain must close its connection too.
    if handler.shared.closing.load(Ordering::Acquire) {
        response.headers_mut().insert(
            http::header::CONNECTION,
            http::HeaderValue::from_static("close"),
        );
    }
    // Track the body sent to hyper after all middleware has returned.
    if let Some(length) = framed_length(&method, &response) {
        let body = response.body_mut();
        if length == 0 {
            // hyper writes the head in the poll that takes the response, so the flush count read here is the one before the head.
            body.guard
                .end_flush
                .get_or_init(|| handler.closed.borrow().flushes);
        } else {
            body.transport = Some(Declared {
                remaining: length,
                closed: handler.closed.clone(),
            });
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
    // hyper never polls the body here, whatever the headers say (Server::can_have_body).
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

async fn handle(
    handler: &Conn,
    chain: Option<Service>,
    req: http::Request<Incoming>,
) -> http::Response<RespBody> {
    let reqs_counter: Arc<InflightReqCount> =
        Arc::new(InflightReqCount::init(&handler.shared.inflight));
    let received_at: f64 = rapira_sapi::work::now_unix_f64();
    let (mut parts, incoming) = req.into_parts();

    let authority = match check::check_request(
        &mut parts,
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

    let Some(mut chain) = chain else {
        return serve_php(
            &handler.shared,
            &handler.closed,
            authority,
            reqs_counter,
            parts,
            incoming,
            peer,
        )
        .await;
    };

    parts.extensions.insert(ReqState {
        authority,
        guard: Arc::clone(&reqs_counter),
        peer,
    });
    let body: middleware::Body = incoming.map_err(BoxError::from).boxed_unsync();
    let req = middleware::Request::from_parts(parts, body);

    // The awaited future is the boxed one that call returns: the compiler cannot prove Send for a held Oneshot over this request type.
    // https://github.com/rust-lang/rust/issues/110338
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
        let state = parts
            .extensions
            .remove::<ReqState>()
            .expect("a middleware keeps the request extensions");
        serve_php(
            &self.shared,
            &self.closed,
            state.authority,
            state.guard,
            parts,
            body,
            state.peer,
        )
        .await
        .map(BodyExt::boxed_unsync)
    }
}

/// The fields of the reply's `Frame::Head`.
struct ReplyHead {
    head: ResponseHead,
    content_length: Option<u64>,
    bodiless: bool,
}

async fn serve_php<B>(
    shared: &Shared,
    closed: &tokio::sync::watch::Receiver<bridge::ConnectionState>,
    authority: Option<Vec<u8>>,
    guard: Arc<InflightReqCount>,
    parts: http::request::Parts,
    mut body: B,
    peer: Peer,
) -> http::Response<RespBody>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let cfg = &shared.cfg;
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

    let ReplyHead {
        head: ResponseHead { status, headers },
        content_length,
        bodiless,
    } = match reply.recv().await {
        None => {
            tracing::error!(target: "http", "php worker died before a response head");
            return refused(http::StatusCode::BAD_GATEWAY, guard);
        }
        Some(Frame::Head {
            head,
            content_length,
            bodiless,
        }) => ReplyHead {
            head,
            content_length,
            bodiless,
        },
        // Every producer sends the head first, so a body frame here means the head is gone.
        Some(Frame::End { .. } | Frame::Chunk(_) | Frame::File { .. }) => {
            tracing::error!(target: "http", "php produced no response head");
            return refused(http::StatusCode::BAD_GATEWAY, guard);
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

    let kind = if bodiless {
        bridge::spawn_drain(reply, closed.clone(), Arc::clone(&guard));
        BodyKind::Empty
    } else {
        let staged = if content_length.is_some() {
            timeout_lazy(Duration::from_millis(10), reply.recv())
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        BodyKind::Reply(bridge::ReplyBody::new(
            reply,
            content_length,
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
    // A HEAD response keeps the declared length. RFC 9110 §8.6 forbids it on a 204, and on a 304 it must equal the 200 length. https://www.rfc-editor.org/rfc/rfc9110#section-8.6
    let header_cl = content_length.filter(|_| !matches!(status.as_u16(), 204 | 304));
    *res.headers_mut() = response_headers(headers, header_cl);
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
    // request::build set the length from the collected body, so it is never -1.
    let bytes = request.content_length as usize;
    let request = parse_multipart(request, shared.cfg.uploads.as_ref()).await?;
    let (tx, reply) = tokio::sync::mpsc::channel(crate::exchange::FRAME_CAP);
    // The classic and worker modes get their CGI view built here, on the transport thread.
    let unit = if shared.cfg.superglobals {
        Unit::Cgi(Box::new(rapira_sapi::types::Context::new(request, tx)))
    } else {
        Unit::Dispatcher(Box::new(Exchange::new(request, tx)))
    };
    shared.intake.submit(unit, bytes).await?;
    Ok(reply)
}

/// The largest multipart body that the IO thread parses. The parse and the spool writes keep a thread for about 0.1 to 0.25 ms per MiB, and each file part adds one file create of about 15 us on xfs. During that time the other connections on that IO thread wait.
const INLINE_PARSE_MAX: usize = 256 << 10;

/// Parses a multipart body before submit, so a rejected body never reaches the pending and active counters. `limits` is None outside dispatcher mode.
/// A body up to INLINE_PARSE_MAX is parsed on the IO thread, which saves the switch to the blocking pool. A larger body is parsed on the blocking pool.
async fn parse_multipart(
    mut req: Request,
    limits: Option<&multipart::Limits>,
) -> Result<Request, Rejection> {
    let Some(limits) = limits else {
        return Ok(req);
    };
    // Content-Type is a singleton field: with repeated lines, the plugin and a PHP consumer could split the body on different boundaries. https://www.rfc-editor.org/rfc/rfc9110#section-8.3
    let lines = req.headers.get_all(CONTENT_TYPE);
    if lines.iter().nth(1).is_some() && lines.iter().any(|v| multipart::is_multipart(v.as_bytes()))
    {
        return Err(Rejection {
            status: http::StatusCode::BAD_REQUEST,
            reason: "repeated content-type field lines with a multipart body".into(),
        });
    }
    let rapira_sapi::types::Body::Raw(raw) = &mut req.body else {
        return Ok(req);
    };
    if raw.get_ref().is_empty() {
        return Ok(req);
    }
    let Some(content_type) = req.content_type.as_deref() else {
        return Ok(req);
    };
    if !multipart::is_multipart(content_type) {
        return Ok(req);
    }
    let boundary = multipart::boundary(content_type)?;
    if raw.get_ref().len() <= INLINE_PARSE_MAX {
        let parsed = multipart::parse(raw.get_ref(), &boundary, limits)?;
        req.body = rapira_sapi::types::Body::Multipart(parsed);
        return Ok(req);
    }
    let bytes = std::mem::take(raw.get_mut());
    let limits = limits.clone();
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
    use tower::service_fn;

    use crate::response::empty_body;

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
