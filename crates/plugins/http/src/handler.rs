//! This module serves one request. It has the admission checks, the middleware chain, the body read, the submit to the intake and the response head.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use http::header::CONTENT_TYPE;
use http_body_util::BodyExt;
use hyper::body::{Body, Bytes, Incoming, SizeHint};
use rapira_sapi::types::{Chunks, ResponseHead};
use rapira_sapi::work::{Intake, Refused, Unit};
use rapira_sapi::{Addr, Frame, Request};
use tower::{Service as _, ServiceExt as _};

use crate::bridge::{self, InflightRequest};
use crate::check::{self, Rejection};
use crate::middleware::{self, BoxError, Layer, Peer, Service};
use crate::php::ExchangeState;
use crate::response::{error_response, response_headers};
use crate::{Config, multipart, request};

pub(crate) struct Shared {
    pub cfg: Config,
    pub intake: Intake,
    pub inflight: Arc<bridge::Inflight>,
    /// True while the listener drains. Each response then closes its connection.
    pub closing: AtomicBool,
}

impl From<Refused> for Rejection {
    fn from(e: Refused) -> Self {
        Self {
            status: http::StatusCode::SERVICE_UNAVAILABLE,
            reason: e.to_string(),
        }
    }
}

/// Per-request values that travel to [`Conn::serve`] through the request extensions.
#[derive(Clone)]
struct ReqState {
    authority: Option<Vec<u8>>,
    guard: Arc<InflightRequest>,
    peer: Peer,
}

pub(crate) struct RespBody {
    kind: BodyKind,
    guard: Arc<InflightRequest>,
    /// [`respond`] arms it once every middleware has returned.
    transport: Option<Declared>,
}

struct Declared {
    /// The declared body bytes that have not passed yet.
    remaining: u64,
    /// The connection state. The body reads it when the last byte passes.
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

fn refused(status: http::StatusCode, req_count: Arc<InflightRequest>) -> http::Response<RespBody> {
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
                this.guard.record(closed);
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
    // The function reads the flag after the request, because a request that ends during the drain must close its connection too.
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
            body.guard.record(&handler.closed);
        } else {
            body.transport = Some(Declared {
                remaining: length,
                closed: handler.closed.clone(),
            });
        }
    }
    response
}

/// Returns the body length that hyper frames, in the order that hyper's h1 encoder (`proto/h1/role.rs`, `Server::encode`) uses:
/// zero when the method or status forbids a body, else the content-length header, else zero for an ended stream, else the exact size hint.
/// `None` means chunked, which needs no record, because a chunked PHP reply ends only after PHP sends End.
/// The ended-stream check also works with body combinators that drop the size hint.
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
    let reqs_counter: Arc<InflightRequest> =
        Arc::new(InflightRequest::init(&handler.shared.inflight));
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

    // The awaited future is the boxed one that call returns, because the compiler cannot prove Send for a held Oneshot over this request type.
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

    /// Returns the configured middleware around [`Conn::serve`], or None without middleware.
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
    guard: Arc<InflightRequest>,
    parts: http::request::Parts,
    mut body: B,
    peer: Peer,
) -> http::Response<RespBody>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let cfg = &shared.cfg;
    let mut collected: Vec<Bytes> = Vec::new();
    // A kept frame holds hyper's whole read buffer, and hyper then reads the next frame into a new buffer of at least 8 KiB. A client that sends a body in small reads can make the worker allocate many times the body size. So the host copies each frame below KEEP_FRAME_MIN into one buffer with its neighbors.
    let mut small: Vec<u8> = Vec::new();
    // A repeated Content-Type refuses a request with or without a body. The first body byte opens the form, so an empty body takes the string arm of the contract.
    let mut form = cfg
        .uploads
        .as_ref()
        .and_then(|_| repeated_content_type(&parts.headers))
        .map(Form::Refused);
    let mut received = 0;
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
                // The host drops non-data frames, that is, request trailers, because PHP has no API for them.
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                received += data.len();
                if received > cfg.max_body_size {
                    tracing::warn!(target: "http", "request body exceeds max_body_size");
                    return refused(http::StatusCode::PAYLOAD_TOO_LARGE, guard);
                }
                if data.is_empty() {
                    continue;
                }
                let open = || Form::open(&parts.headers, cfg.uploads.as_ref());
                match form.get_or_insert_with(open) {
                    Form::Multipart(parser) => {
                        if let Err(r) = parser.feed(&data) {
                            form = Some(Form::Refused(r));
                        }
                    }
                    // The host reads the rest of the body, so the client gets the refusal as a complete response.
                    Form::Refused(_) => {}
                    Form::Raw if data.len() < KEEP_FRAME_MIN => small.extend_from_slice(&data),
                    Form::Raw => {
                        if !small.is_empty() {
                            collected.push(Bytes::from(std::mem::take(&mut small)));
                        }
                        collected.push(data);
                    }
                }
            }
            Some(Err(e)) => {
                tracing::debug!(target: "http", "request body read failed: {e}");
                return refused(http::StatusCode::BAD_REQUEST, guard);
            }
        }
    }
    if !small.is_empty() {
        collected.push(Bytes::from(small));
    }

    let multipart = match form {
        Some(Form::Multipart(parser)) => parser.finish().map(Some),
        Some(Form::Refused(r)) => Err(r),
        Some(Form::Raw) | None => Ok(None),
    };
    let request = request::build(parts, authority, Chunks::new(collected), peer);
    let submitted = match multipart {
        Ok(multipart) => {
            // The file parts are in the spool directory, so the body budget counts only the rest of the body.
            let mut held = received;
            if let Some(body) = &multipart {
                for file in &body.files {
                    held -= file.size as usize;
                }
            }
            submit(shared, request, multipart, held).await
        }
        Err(r) => Err(r),
    };
    let mut reply = match submitted {
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
        // Every producer sends the head first, so a body frame here means that the head is missing.
        Some(Frame::End { .. } | Frame::Chunk(_) | Frame::File { .. }) => {
            tracing::error!(target: "http", "php produced no response head");
            return refused(http::StatusCode::BAD_GATEWAY, guard);
        }
    };

    let status = match http::StatusCode::from_u16(status) {
        Ok(s) if s.as_u16() >= 200 => s,
        _ => {
            // hyper rewrites a 1xx status from the service to 500 and fails the
            // connection. A 502 head keeps the connection coherent.
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

/// Polls `fut` once and arms the timer only when it is pending. A ready future needs no timer.
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

/// A cap of 16 lets a buffered Head, Chunk and End queue without a park of the PHP thread. It also keeps a 1 MiB body in 64 KiB writes to about one park. tokio allocates one 32-slot block for any cap up to 32.
const FRAME_CAP: usize = 16;

/// A body frame at or above this size travels as hyper delivered it. The host copies a smaller frame into one buffer with its neighbors. hyper reads up to 64 KiB per frame, so a bulk body keeps its frames.
const KEEP_FRAME_MIN: usize = 16 << 10;

/// `bytes` is the number of body bytes that the host holds in memory. The intake reserves it in the body budget of the pool.
async fn submit(
    shared: &Shared,
    request: Request,
    multipart: Option<multipart::MultipartBody>,
    bytes: usize,
) -> Result<tokio::sync::mpsc::Receiver<Frame>, Rejection> {
    let (tx, reply) = tokio::sync::mpsc::channel(FRAME_CAP);
    // The host builds the CGI view of the classic and worker modes here, on the IO thread.
    let unit = if shared.cfg.superglobals {
        Unit::Cgi(Box::new(rapira_sapi::types::Context::new(request, tx)))
    } else {
        Unit::Dispatcher(Box::new(ExchangeState::new(request, multipart, tx)))
    };
    shared.intake.submit(unit, bytes).await?;
    Ok(reply)
}

/// The body of a request while it arrives. Dispatcher mode parses a multipart form before submit, so a refused body never reaches the pending and active counters.
enum Form<'a> {
    /// PHP gets the raw body.
    Raw,
    /// The parser takes each frame when it arrives, so the host never holds the body. It runs on the IO thread, where the bytes of the frame are in the CPU cache.
    Multipart(Box<multipart::Parser<'a>>),
    Refused(Rejection),
}

impl<'a> Form<'a> {
    /// `limits` is None outside dispatcher mode.
    fn open(headers: &http::HeaderMap, limits: Option<&'a multipart::Limits>) -> Self {
        let Some(limits) = limits else {
            return Self::Raw;
        };
        let Some(content_type) = headers.get(CONTENT_TYPE) else {
            return Self::Raw;
        };
        if !multipart::is_multipart(content_type.as_bytes()) {
            return Self::Raw;
        }
        match multipart::boundary(content_type.as_bytes()) {
            Ok(boundary) => Self::Multipart(Box::new(multipart::Parser::new(&boundary, limits))),
            Err(r) => Self::Refused(r),
        }
    }
}

/// Content-Type is a singleton field. With repeated lines, the plugin and a PHP consumer could split the body on different boundaries. https://www.rfc-editor.org/rfc/rfc9110#section-8.3
fn repeated_content_type(headers: &http::HeaderMap) -> Option<Rejection> {
    let lines = headers.get_all(CONTENT_TYPE);
    let repeated = lines.iter().nth(1).is_some()
        && lines.iter().any(|v| multipart::is_multipart(v.as_bytes()));
    repeated.then(|| Rejection {
        status: http::StatusCode::BAD_REQUEST,
        reason: "repeated content-type field lines with a multipart body".into(),
    })
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

    /// A future that is pending for `left` polls, then ready. Each pending poll wakes the task at once.
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

    /// Outside a runtime, `tokio::time::timeout` panics, so a ready future must complete without it.
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
