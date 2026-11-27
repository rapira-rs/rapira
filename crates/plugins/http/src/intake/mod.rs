//! Inbound HTTP request admission, Phase-1 middleware execution, body/multipart reading, and SAPI `Request` construction.

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName};
use http::{Extensions, HeaderMap, Method, Request as HttpRequest, StatusCode, Uri, Version};
use http_body::Body;
use http_body_util::BodyExt;
use rapira_http_middleware::{
    MiddlewareChain, Peer, PeerAddr, RequestCtx, RequestHeaders, ShortResponse,
};
use rapira_sapi::types::{Authority, Chunks, RequestUri};
use rapira_sapi::{Addr, Request};

use crate::Rejection;
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
#[allow(clippy::large_enum_variant)]
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
pub fn is_safe_field_name(name: &str) -> bool {
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
) -> Result<AdmittedHead, Rejection> {
    let mut hosts = parts.headers.get_all(HOST).iter();
    let first_host = hosts.next();
    if hosts.next().is_some() {
        return Err(Rejection {
            status: StatusCode::BAD_REQUEST,
            reason: "repeated Host header".into(),
        });
    }
    let host_bytes = first_host.map(|v| v.as_bytes().trim_ascii());
    let uri_authority = parts.uri.authority().map(|a| a.as_str().as_bytes());

    let authority = match host_bytes {
        Some(b"") if parts.version == Version::HTTP_11 => {
            return Err(Rejection {
                status: StatusCode::BAD_REQUEST,
                reason: "empty Host header".into(),
            });
        }
        None if parts.version == Version::HTTP_11 => {
            return Err(Rejection {
                status: StatusCode::BAD_REQUEST,
                reason: "missing Host header".into(),
            });
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
        return Err(Rejection {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            reason: format!("request body of {cl} bytes exceeds max_body_size ({max_body_size})"),
        });
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
    headers: RequestHeaders,
    authority: Authority,
    body: Chunks,
    peer: Peer,
) -> Request {
    let protocol = match version {
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_2 => "HTTP/2.0",
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

enum BodyMode<'a> {
    Raw {
        /// Holds the first frame inline without allocating `Vec` or `BytesMut` for single-frame bodies.
        first: Option<Bytes>,
        parts: Vec<Bytes>,
        small: BytesMut,
    },
    /// Opened lazily on the first non-empty body byte so an empty POST reaches PHP raw.
    Multipart {
        content_type: &'a [u8],
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

/// Rejects repeated multipart `Content-Type` headers before polling the body; defers boundary parsing until the first non-empty frame.
fn open_body_mode<'a>(
    headers: &'a HeaderMap,
    uploads: Option<&'a multipart::Limits>,
    content_length: Option<u64>,
) -> Result<BodyMode<'a>, Rejection> {
    let Some(limits) = uploads else {
        return Ok(BodyMode::Raw {
            first: None,
            parts: Vec::new(),
            small: BytesMut::new(),
        });
    };

    let content_type = match inspect_multipart_ct(headers) {
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
    let size_hint = content_length.and_then(|n| usize::try_from(n).ok());
    Ok(BodyMode::Multipart {
        content_type,
        limits,
        size_hint,
        parser: None,
    })
}

struct ReadBody {
    chunks: Chunks,
    multipart: Option<Box<MultipartBody>>,
    budget_bytes: usize,
}

/// Rejects repeated multipart `Content-Type` headers without constructing `BodyMode` on empty bodies.
fn reject_repeated_multipart(
    headers: &HeaderMap,
    uploads: Option<&multipart::Limits>,
) -> Result<(), Rejection> {
    if uploads.is_some() && matches!(inspect_multipart_ct(headers), MultipartHeader::Repeated) {
        return Err(Rejection {
            status: StatusCode::BAD_REQUEST,
            reason: "repeated Content-Type header on multipart request".into(),
        });
    }
    Ok(())
}

async fn read_request_body<B>(
    mut body: B,
    mut mode: BodyMode<'_>,
    max_body_size: usize,
    timeout: Duration,
) -> Result<ReadBody, Rejection>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: fmt::Display,
{
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
                content_type,
                limits,
                size_hint,
                parser,
            } => {
                let p = match parser {
                    Some(p) => p,
                    None => {
                        let boundary = multipart::boundary(content_type)?;
                        parser.insert(Box::new(multipart::Parser::new_with_hint(
                            &boundary, limits, *size_hint,
                        )))
                    }
                };
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
) -> Result<Admit<B>, Rejection> {
    let (mut parts, body) = req.into_parts();
    let admitted = check_request(&mut parts, limits.superglobals, limits.max_body_size)?;

    if !chain.has_request() && body.is_end_stream() {
        reject_repeated_multipart(&parts.headers, limits.uploads)?;
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
    B::Error: fmt::Display,
{
    /// Completes Phase-1 middleware, body/multipart reading, and SAPI `Request` construction.
    pub(crate) async fn finish(
        self,
        chain: &MiddlewareChain,
        limits: &IntakeLimits<'_>,
    ) -> Result<Prepared, Rejection> {
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
            reject_repeated_multipart(&headers, limits.uploads)?;
            ReadBody {
                chunks: Chunks::default(),
                multipart: None,
                budget_bytes: 0,
            }
        } else {
            let mode = open_body_mode(&headers, limits.uploads, admitted.content_length)?;
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
