//! Inbound HTTP request admission, Phase-1 middleware execution, body/multipart reading, and SAPI `Request` construction.

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName};
use http::{Extensions, HeaderMap, Method, Request as HttpRequest, StatusCode, Uri, Version};
use http_body::Body;
use http_body_util::BodyExt;
use rapira_http_middleware::{MiddlewareChain, Peer, PeerAddr, RequestCtx, ShortResponse};
use rapira_sapi::types::{Authority, Chunks, RequestUri};
use rapira_sapi::{Addr, Request};

use crate::multipart::{self, MultipartBody};

/// Frames smaller than this coalesce into one buffer when multiple frames arrive.
const COALESCE_MAX: usize = 16 << 10;

/// Validated request head metadata produced once before middleware or body reads.
#[derive(Debug)]
pub struct AdmittedHead {
    pub authority: Authority,
    pub content_length: Option<u64>,
}

/// Limits applied by `prepare` during head admission and body reading.
pub(crate) struct IntakeLimits<'a> {
    pub superglobals: bool,
    pub max_body_size: usize,
    pub body_timeout: Duration,
    pub uploads: Option<&'a multipart::Limits>,
}

/// Result of synchronous head admission: either ready immediately on the empty-body fast path, or pending async work.
pub(crate) enum Admit<B> {
    Ready(Request),
    Pending(PendingIntake<B>),
}

/// Request waiting on Phase-1 middleware or non-empty body reads.
pub(crate) struct PendingIntake<B> {
    parts: http::request::Parts,
    body: B,
    admitted: AdmittedHead,
    peer: Peer,
}

/// Result of completing asynchronous intake before SAPI submission.
// Returned by value and immediately consumed; boxing `Request` adds a per-POST allocation.
#[expect(clippy::large_enum_variant)]
pub(crate) enum Prepared {
    Short {
        outer_depth: usize,
        short: ShortResponse,
        extensions: Extensions,
    },
    Dispatch {
        request: Request,
        multipart: Option<Box<MultipartBody>>,
        /// Bytes counted toward the SAPI pool body budget (excludes spooled upload files).
        budget_bytes: usize,
        extensions: Extensions,
    },
}

/// Parses a non-empty ASCII decimal `Content-Length` without signs or overflow.
pub fn parse_content_length(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() {
        return None;
    }
    let mut val: u64 = 0;
    for &b in raw {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(val)
}

/// True when `name` contains only ASCII letters, digits, and `-`, so `HTTP_*` CGI mangling cannot alias another header.
#[inline]
fn is_safe_field_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Drops request headers whose names could alias a CGI `HTTP_*` variable.
pub fn drop_unsafe_field_names(headers: &mut HeaderMap) {
    if headers.keys().all(|k| is_safe_field_name(k.as_str())) {
        return;
    }
    let bad: Vec<HeaderName> = headers
        .keys()
        .filter(|k| !is_safe_field_name(k.as_str()))
        .cloned()
        .collect();
    for key in bad {
        headers.remove(&key);
    }
}

/// Validates Host and Content-Length before middleware or body reads, and extracts authority and declared length.
pub fn check_request(
    parts: &mut http::request::Parts,
    superglobals: bool,
    max_body_size: usize,
) -> Result<AdmittedHead, StatusCode> {
    let mut hosts = parts.headers.get_all(HOST).iter();
    let first_host = hosts.next();
    if hosts.next().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let host_bytes = first_host.map(|v| v.as_bytes().trim_ascii());
    let uri_authority = parts.uri.authority().map(|a| a.as_str().as_bytes());

    let authority = match host_bytes {
        Some(b"") if parts.version == Version::HTTP_11 => {
            return Err(StatusCode::BAD_REQUEST);
        }
        None if parts.version == Version::HTTP_11 => {
            return Err(StatusCode::BAD_REQUEST);
        }
        Some(b) if !b.is_empty() => Authority::HostHeader,
        _ => match uri_authority {
            Some(a) if !a.is_empty() => Authority::Explicit(Bytes::copy_from_slice(a)),
            _ => Authority::None,
        },
    };

    let content_length = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| parse_content_length(v.as_bytes().trim_ascii()));
    if let Some(cl) = content_length
        && cl > max_body_size as u64
    {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    if superglobals {
        drop_unsafe_field_names(&mut parts.headers);
    }

    Ok(AdmittedHead {
        authority,
        content_length,
    })
}

fn peer_addr(addr: PeerAddr) -> Addr {
    match addr {
        PeerAddr::Inet(sa) => Addr::Inet(sa),
        PeerAddr::Unix(p) => Addr::Unix(p),
    }
}

fn method_cow(m: &Method) -> Cow<'static, str> {
    match *m {
        Method::GET => Cow::Borrowed("GET"),
        Method::POST => Cow::Borrowed("POST"),
        Method::PUT => Cow::Borrowed("PUT"),
        Method::DELETE => Cow::Borrowed("DELETE"),
        Method::HEAD => Cow::Borrowed("HEAD"),
        Method::OPTIONS => Cow::Borrowed("OPTIONS"),
        Method::PATCH => Cow::Borrowed("PATCH"),
        Method::CONNECT => Cow::Borrowed("CONNECT"),
        Method::TRACE => Cow::Borrowed("TRACE"),
        _ => Cow::Owned(m.as_str().to_owned()),
    }
}

/// Builds the SAPI `Request` from the parsed HTTP head, raw body chunks, and peer metadata.
pub fn build_request(
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
    authority: Authority,
    body: Chunks,
    peer: Peer,
) -> Request {
    let protocol = match version {
        Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    };
    let target = uri.authority().map(|_| uri.to_string().into_bytes());
    let req_uri = match uri.path_and_query() {
        None => RequestUri::Static("/"),
        Some(pq) => match pq.as_str() {
            "/" => RequestUri::Static("/"),
            s if s.starts_with('/') || s == "*" => RequestUri::Parsed(uri),
            // Display restores the leading slash that an authority-only absolute-form omits, so "http://h?q" gives "/?q".
            // https://github.com/hyperium/http/blob/v1.5.0/src/uri/path.rs#L277-L291
            _ => RequestUri::Owned(pq.to_string()),
        },
    };
    Request {
        method: method_cow(&method),
        uri: req_uri,
        // For the origin and asterisk forms the target equals uri, which PHP uses when this is None.
        target,
        authority,
        https: peer.https,
        protocol,
        remote: peer_addr(peer.remote),
        server: peer_addr(peer.server),
        received_at: peer.received_at,
        body,
        headers,
    }
}

/// Polls `fut` once immediately; allocates or resets `sleep` only if the first poll returns `Pending`.
async fn timeout_lazy<F: Future>(
    dur: Duration,
    sleep: &mut Option<Pin<Box<tokio::time::Sleep>>>,
    fut: F,
) -> Result<F::Output, ()> {
    let mut fut = std::pin::pin!(fut);
    let mut armed = false;
    std::future::poll_fn(|cx| {
        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
            return Poll::Ready(Ok(out));
        }
        let s = match sleep {
            Some(s) => {
                if !armed {
                    s.as_mut().reset(tokio::time::Instant::now() + dur);
                    armed = true;
                }
                s
            }
            None => {
                armed = true;
                sleep.insert(Box::pin(tokio::time::sleep(dur)))
            }
        };
        if s.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(()));
        }
        Poll::Pending
    })
    .await
}

enum SpoolMsg {
    Data(Bytes),
    Finish,
}

enum MultipartState {
    Idle,
    Streaming {
        tx: tokio::sync::mpsc::Sender<SpoolMsg>,
        handle: tokio::task::JoinHandle<Result<MultipartBody, StatusCode>>,
    },
    Done(MultipartBody),
}

enum BodyMode<'a> {
    Raw {
        /// Holds the first frame inline without allocating `Vec` or `BytesMut` for single-frame bodies.
        first: Option<Bytes>,
        parts: Vec<Bytes>,
        small: BytesMut,
    },
    /// Opened lazily on the first non-empty body byte so an empty POST reaches PHP raw.
    /// Runs `multipart::Parser` on the blocking thread pool so file spooling and error-path unlinks never block the IO shard.
    Multipart {
        content_type: &'a [u8],
        limits: &'a multipart::Limits,
        size_hint: Option<usize>,
        state: MultipartState,
    },
}

/// Returns the `Content-Type` bytes for a single multipart upload header, rejecting repeated multipart `Content-Type` headers.
fn multipart_content_type<'a>(
    headers: &'a HeaderMap,
    uploads: Option<&multipart::Limits>,
) -> Result<Option<&'a [u8]>, StatusCode> {
    if uploads.is_none() {
        return Ok(None);
    }
    let mut cts = headers.get_all(CONTENT_TYPE).iter();
    let Some(first) = cts.next() else {
        return Ok(None);
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
        Ok(None)
    } else if has_second {
        Err(StatusCode::BAD_REQUEST)
    } else {
        Ok(Some(first_bytes))
    }
}

struct ReadBody {
    chunks: Chunks,
    multipart: Option<Box<MultipartBody>>,
    budget_bytes: usize,
}

async fn read_request_body<B>(
    mut body: B,
    mut mode: BodyMode<'_>,
    max_body_size: usize,
    timeout: Duration,
) -> Result<ReadBody, StatusCode>
where
    B: Body<Data = Bytes> + Unpin,
{
    let mut total: usize = 0;
    let mut sleep: Option<Pin<Box<tokio::time::Sleep>>> = None;

    while let Some(frame_res) = match timeout_lazy(timeout, &mut sleep, body.frame()).await {
        Ok(next) => next,
        Err(()) => return Err(StatusCode::REQUEST_TIMEOUT),
    } {
        let frame = frame_res.map_err(|_| StatusCode::BAD_REQUEST)?;
        let Ok(data) = frame.into_data() else {
            // Request trailers are ignored (PHP has no request trailer API).
            continue;
        };
        if data.is_empty() {
            continue;
        }
        total = total.saturating_add(data.len());
        if total > max_body_size {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
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
                content_type,
                limits,
                size_hint,
                state,
            } => match state {
                MultipartState::Idle => {
                    let boundary = multipart::boundary(content_type)?;
                    let limits = (*limits).clone();
                    let hint = *size_hint;
                    if body.is_end_stream() {
                        let mp = tokio::task::spawn_blocking(move || {
                            let mut parser = multipart::Parser::new_owned(&boundary, limits, hint);
                            parser.feed(&data)?;
                            parser.finish()
                        })
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
                        *state = MultipartState::Done(mp);
                        break;
                    }
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<SpoolMsg>(4);
                    let handle = tokio::task::spawn_blocking(move || {
                        let mut parser = multipart::Parser::new_owned(&boundary, limits, hint);
                        while let Some(msg) = rx.blocking_recv() {
                            match msg {
                                SpoolMsg::Data(chunk) => parser.feed(&chunk)?,
                                SpoolMsg::Finish => return parser.finish(),
                            }
                        }
                        // Dropping `parser` here unlinks any partial spool files on the blocking thread.
                        Err(StatusCode::BAD_REQUEST)
                    });
                    if tx.send(SpoolMsg::Data(data)).await.is_err() {
                        handle
                            .await
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                    *state = MultipartState::Streaming { tx, handle };
                }
                MultipartState::Streaming { tx, handle } => {
                    if tx.send(SpoolMsg::Data(data)).await.is_err() {
                        handle
                            .await
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
                MultipartState::Done(_) => {}
            },
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
        BodyMode::Multipart {
            state: MultipartState::Idle,
            ..
        } => Ok(ReadBody {
            chunks: Chunks::default(),
            multipart: None,
            budget_bytes: 0,
        }),
        BodyMode::Multipart { state, .. } => {
            let mp = match state {
                MultipartState::Done(mp) => mp,
                MultipartState::Streaming { tx, handle } => {
                    let _ = tx.send(SpoolMsg::Finish).await;
                    drop(tx);
                    handle
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??
                }
                MultipartState::Idle => unreachable!(),
            };
            let spooled: usize = mp
                .files
                .iter()
                .map(|f| usize::try_from(f.size).unwrap_or(usize::MAX))
                .fold(0usize, usize::saturating_add);
            let budget_bytes = total.saturating_sub(spooled);
            Ok(ReadBody {
                chunks: Chunks::default(),
                multipart: Some(Box::new(mp)),
                budget_bytes,
            })
        }
    }
}

/// Runs synchronous head admission; returns `Admit::Ready` immediately when no Phase-1 middleware or body read is needed.
pub(crate) fn admit<B: Body>(
    req: HttpRequest<B>,
    peer: Peer,
    chain: &MiddlewareChain,
    limits: &IntakeLimits<'_>,
) -> Result<Admit<B>, StatusCode> {
    let (mut parts, body) = req.into_parts();
    let admitted = check_request(&mut parts, limits.superglobals, limits.max_body_size)?;

    if !chain.has_request() && body.is_end_stream() {
        multipart_content_type(&parts.headers, limits.uploads)?;
        let request = build_request(
            parts.method,
            parts.uri,
            parts.version,
            parts.headers,
            admitted.authority,
            Chunks::default(),
            peer,
        );
        return Ok(Admit::Ready(request));
    }

    Ok(Admit::Pending(PendingIntake {
        parts,
        body,
        admitted,
        peer,
    }))
}

impl<B> PendingIntake<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    /// Completes Phase-1 middleware, body/multipart reading, and SAPI `Request` construction.
    pub(crate) async fn finish(
        self,
        chain: &MiddlewareChain,
        limits: &IntakeLimits<'_>,
    ) -> Result<Prepared, StatusCode> {
        let Self {
            parts,
            body,
            admitted,
            peer,
        } = self;

        let (method, uri, version, headers, peer, extensions) = if chain.has_request() {
            let mut ctx = RequestCtx {
                method: parts.method,
                uri: parts.uri,
                version: parts.version,
                headers: parts.headers,
                peer,
                extensions: Extensions::new(),
            };
            if let Some((outer_depth, short)) = chain.run_request(&mut ctx).await {
                return Ok(Prepared::Short {
                    outer_depth,
                    short,
                    extensions: ctx.extensions,
                });
            }
            (
                ctx.method,
                ctx.uri,
                ctx.version,
                ctx.headers,
                ctx.peer,
                ctx.extensions,
            )
        } else {
            (
                parts.method,
                parts.uri,
                parts.version,
                parts.headers,
                peer,
                Extensions::new(),
            )
        };

        let read = if body.is_end_stream() {
            multipart_content_type(&headers, limits.uploads)?;
            ReadBody {
                chunks: Chunks::default(),
                multipart: None,
                budget_bytes: 0,
            }
        } else {
            let mode = match (
                multipart_content_type(&headers, limits.uploads)?,
                limits.uploads,
            ) {
                (Some(content_type), Some(uploads)) => BodyMode::Multipart {
                    content_type,
                    limits: uploads,
                    size_hint: admitted
                        .content_length
                        .and_then(|n| usize::try_from(n).ok()),
                    state: MultipartState::Idle,
                },
                _ => BodyMode::Raw {
                    first: None,
                    parts: Vec::new(),
                    small: BytesMut::new(),
                },
            };
            read_request_body(body, mode, limits.max_body_size, limits.body_timeout).await?
        };

        let request = build_request(
            method,
            uri,
            version,
            headers,
            admitted.authority,
            read.chunks,
            peer,
        );
        Ok(Prepared::Dispatch {
            request,
            multipart: read.multipart,
            budget_bytes: read.budget_bytes,
            extensions,
        })
    }
}
