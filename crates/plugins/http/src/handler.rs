//! Per-request HTTP admission, middleware, body/multipart reading, and SAPI dispatch on one IO shard.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{Extensions, HeaderMap, Method, Request as HttpRequest, Response, StatusCode};
use http_body::Body as _;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use rapira_http_middleware::{Peer, RequestCtx};
use rapira_sapi::types::{Chunks, Context as SapiContext};
use rapira_sapi::work::{Refused, Unit};

use crate::check::{check_request, parse_content_length};
use crate::multipart::{self, MultipartBody};
use crate::php::ExchangeState;
use crate::responder::{
    HttpResponder, RequestGuard, ResponseBody, build_php_response, build_short_response, refusal,
};
use crate::serve::ConnCtx;
use crate::{Rejection, request};

/// Frames smaller than this coalesce into one buffer when multiple frames arrive.
const COALESCE_MAX: usize = 16 << 10;

/// Polls `fut` once immediately; allocates a `tokio::time::Sleep` only if the first poll returns `Pending`.
async fn timeout_lazy<F: Future>(dur: Duration, fut: F) -> Result<F::Output, ()> {
    let mut fut = std::pin::pin!(fut);
    let mut sleep: Option<Pin<Box<tokio::time::Sleep>>> = None;
    std::future::poll_fn(|cx| {
        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
            return Poll::Ready(Ok(out));
        }
        let s = sleep.get_or_insert_with(|| Box::pin(tokio::time::sleep(dur)));
        if s.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(()));
        }
        Poll::Pending
    })
    .await
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

enum BodyMode<'a> {
    Raw {
        /// Holds the first frame inline without allocating `Vec` or `BytesMut` for single-frame bodies.
        first: Option<Bytes>,
        parts: Vec<Bytes>,
        small: BytesMut,
    },
    /// Opened lazily on the first non-empty body byte so an empty POST reaches PHP raw.
    Multipart {
        boundary: Vec<u8>,
        limits: &'a multipart::Limits,
        size_hint: Option<usize>,
        parser: Option<Box<multipart::Parser<'a>>>,
    },
}

enum MultipartHeader<'a> {
    None,
    Repeated,
    Single(&'a [u8]),
}

/// Inspects `Content-Type` lines once for both empty-body validation and multipart parser setup.
fn inspect_multipart_ct(headers: &HeaderMap) -> MultipartHeader<'_> {
    let mut cts = headers.get_all(CONTENT_TYPE).iter();
    let Some(first) = cts.next() else {
        return MultipartHeader::None;
    };
    let first_bytes = first.as_bytes();
    let mut has_second = false;
    let mut any_multipart = multipart::is_multipart(first_bytes);
    for extra in cts {
        has_second = true;
        if multipart::is_multipart(extra.as_bytes()) {
            any_multipart = true;
        }
    }
    if !any_multipart {
        MultipartHeader::None
    } else if has_second {
        MultipartHeader::Repeated
    } else {
        MultipartHeader::Single(first_bytes)
    }
}

/// Checks for repeated `Content-Type` headers where any line is `multipart/form-data`.
fn has_repeated_multipart_ct(headers: &HeaderMap, uploads: Option<&multipart::Limits>) -> bool {
    uploads.is_some() && matches!(inspect_multipart_ct(headers), MultipartHeader::Repeated)
}

/// Validates multipart `Content-Type` before polling the body so `Expect: 100-continue` is skipped on `400`.
fn open_body_mode<'a>(
    headers: &HeaderMap,
    uploads: Option<&'a multipart::Limits>,
) -> Result<BodyMode<'a>, Rejection> {
    let Some(limits) = uploads else {
        return Ok(BodyMode::Raw {
            first: None,
            parts: Vec::new(),
            small: BytesMut::new(),
        });
    };

    let first_bytes = match inspect_multipart_ct(headers) {
        MultipartHeader::None => {
            return Ok(BodyMode::Raw {
                first: None,
                parts: Vec::new(),
                small: BytesMut::new(),
            });
        }
        MultipartHeader::Repeated => {
            return Err(Rejection {
                status: StatusCode::BAD_REQUEST,
                reason: "repeated Content-Type header on multipart request".into(),
            });
        }
        MultipartHeader::Single(bytes) => bytes,
    };
    let boundary = multipart::boundary(first_bytes)?;
    let size_hint = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| parse_content_length(v.as_bytes().trim_ascii()))
        .and_then(|n| usize::try_from(n).ok());
    Ok(BodyMode::Multipart {
        boundary,
        limits,
        size_hint,
        parser: None,
    })
}

struct ReadBody {
    chunks: Chunks,
    multipart: Option<MultipartBody>,
    /// Bytes counted toward the SAPI pool body budget (excludes spooled file bytes).
    budget_bytes: usize,
}

async fn read_request_body(
    mut body: Incoming,
    headers: &HeaderMap,
    uploads: Option<&multipart::Limits>,
    max_body_size: usize,
    timeout: Duration,
) -> Result<ReadBody, Rejection> {
    let mut mode = open_body_mode(headers, uploads)?;
    let mut total: usize = 0;

    while let Some(frame_res) = match timeout_lazy(timeout, body.frame()).await {
        Ok(next) => next,
        Err(()) => {
            return Err(Rejection {
                status: StatusCode::REQUEST_TIMEOUT,
                reason: "request body read timed out".into(),
            });
        }
    } {
        let frame = frame_res.map_err(|e| Rejection {
            status: StatusCode::BAD_REQUEST,
            reason: format!("request body read failed: {e}"),
        })?;
        let Ok(data) = frame.into_data() else {
            // Request trailers are ignored (PHP has no request trailer API).
            continue;
        };
        if data.is_empty() {
            continue;
        }
        total = total.saturating_add(data.len());
        if total > max_body_size {
            return Err(Rejection {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                reason: format!("request body exceeds max_body_size ({max_body_size})"),
            });
        }
        match &mut mode {
            BodyMode::Raw {
                first,
                parts,
                small,
            } => {
                if parts.is_empty() && small.is_empty() {
                    if let Some(prev) = first.take() {
                        if prev.len() < COALESCE_MAX && data.len() < COALESCE_MAX {
                            small.reserve(prev.len() + data.len());
                            small.extend_from_slice(&prev);
                            small.extend_from_slice(&data);
                            if small.len() >= COALESCE_MAX {
                                parts.push(small.split().freeze());
                            }
                        } else {
                            parts.push(prev);
                            if data.len() < COALESCE_MAX {
                                small.extend_from_slice(&data);
                            } else {
                                parts.push(data);
                            }
                        }
                    } else {
                        *first = Some(data);
                    }
                } else if data.len() < COALESCE_MAX {
                    small.extend_from_slice(&data);
                    if small.len() >= COALESCE_MAX {
                        parts.push(small.split().freeze());
                    }
                } else {
                    if !small.is_empty() {
                        parts.push(small.split().freeze());
                    }
                    parts.push(data);
                }
            }
            BodyMode::Multipart {
                boundary,
                limits,
                size_hint,
                parser,
            } => {
                let p = parser.get_or_insert_with(|| {
                    Box::new(multipart::Parser::new_with_hint(
                        boundary, limits, *size_hint,
                    ))
                });
                p.feed(&data)?;
            }
        }
    }

    match mode {
        BodyMode::Raw {
            first: Some(one), ..
        } => {
            let budget_bytes = one.len();
            Ok(ReadBody {
                chunks: Chunks::single(one),
                multipart: None,
                budget_bytes,
            })
        }
        BodyMode::Raw {
            first: None,
            mut parts,
            mut small,
        } => {
            if !small.is_empty() {
                parts.push(small.split().freeze());
            }
            let chunks = Chunks::new(parts);
            let budget_bytes = chunks.len();
            Ok(ReadBody {
                chunks,
                multipart: None,
                budget_bytes,
            })
        }
        BodyMode::Multipart { parser: None, .. } => Ok(ReadBody {
            chunks: Chunks::default(),
            multipart: None,
            budget_bytes: 0,
        }),
        BodyMode::Multipart {
            parser: Some(parser),
            ..
        } => {
            let mp = parser.finish()?;
            let spooled: usize = mp
                .files
                .iter()
                .map(|f| usize::try_from(f.size).unwrap_or(usize::MAX))
                .fold(0usize, usize::saturating_add);
            let budget_bytes = total.saturating_sub(spooled);
            Ok(ReadBody {
                chunks: Chunks::default(),
                multipart: Some(mp),
                budget_bytes,
            })
        }
    }
}

/// Handles one HTTP request on the connection's IO shard.
pub(crate) async fn handle_request(
    conn: Rc<ConnCtx>,
    req: HttpRequest<Incoming>,
) -> Result<Response<ResponseBody>, std::convert::Infallible> {
    let mut guard = RequestGuard::new(Rc::clone(&conn.phase), Rc::clone(&conn.shard.inflight));
    let shared = &conn.shard.shared;
    let (mut parts, body) = req.into_parts();
    let is_head = parts.method == Method::HEAD;

    let authority = match check_request(
        &mut parts,
        shared.cfg.superglobals,
        shared.cfg.max_body_size,
    ) {
        Ok(auth) => auth,
        Err(rej) => return Ok(refusal(rej.status, guard)),
    };

    let peer = Peer {
        remote: conn.remote.clone(),
        server: conn.server.clone(),
        https: false,
        received_at: now_unix(),
    };

    let (method, uri, version, headers, extensions) = if shared.chain.has_request() {
        let mut ctx = RequestCtx {
            method: parts.method,
            uri: parts.uri,
            version: parts.version,
            headers: parts.headers,
            peer: peer.clone(),
            extensions: Extensions::new(),
        };
        if let Some((outer_depth, short)) = shared.chain.run_request(&mut ctx).await {
            let closing = shared.closing.load(Ordering::Acquire);
            return Ok(build_short_response(
                &shared.chain,
                outer_depth,
                is_head,
                closing,
                &ctx.extensions,
                short,
                guard,
            ));
        }
        (
            ctx.method,
            ctx.uri,
            ctx.version,
            ctx.headers,
            ctx.extensions,
        )
    } else {
        (
            parts.method,
            parts.uri,
            parts.version,
            parts.headers,
            Extensions::new(),
        )
    };

    let read = if body.is_end_stream() {
        if has_repeated_multipart_ct(&headers, shared.cfg.uploads.as_ref()) {
            return Ok(refusal(StatusCode::BAD_REQUEST, guard));
        }
        ReadBody {
            chunks: Chunks::default(),
            multipart: None,
            budget_bytes: 0,
        }
    } else {
        match read_request_body(
            body,
            &headers,
            shared.cfg.uploads.as_ref(),
            shared.cfg.max_body_size,
            shared.cfg.keepalive_timeout,
        )
        .await
        {
            Ok(read) => read,
            Err(rej) => return Ok(refusal(rej.status, guard)),
        }
    };

    let sapi_req = request::build(method, uri, version, headers, authority, read.chunks, peer);

    let (responder, slot) = HttpResponder::pair(is_head);
    guard.attach_slot(slot);

    let unit = if shared.cfg.superglobals {
        Unit::Cgi(Box::new(SapiContext::new(sapi_req, Box::new(responder))))
    } else {
        Unit::Dispatcher(Box::new(ExchangeState::new(
            sapi_req,
            read.multipart,
            responder,
        )))
    };

    if let Err(refused) = shared.intake.submit(unit, read.budget_bytes).await {
        let status = match refused {
            Refused::OverBudget | Refused::Saturated => StatusCode::SERVICE_UNAVAILABLE,
        };
        return Ok(refusal(status, guard));
    }

    match guard.wait_start().await {
        Ok(start) => {
            let closing = shared.closing.load(Ordering::Acquire);
            Ok(build_php_response(
                &shared.chain,
                is_head,
                closing,
                &extensions,
                start,
                guard,
            ))
        }
        Err(_) => Ok(refusal(StatusCode::BAD_GATEWAY, guard)),
    }
}
