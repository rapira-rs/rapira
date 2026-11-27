//! Unified HTTP response head normalizer, wire framing, encoding, and PHP-to-IO handoff.

mod body;
mod slot;
#[cfg(test)]
mod tests;

use bytes::Bytes;
use http::header::{
    CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, HeaderName, HeaderValue, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use http::{Extensions, HeaderMap, Response, StatusCode};
use rapira_http_middleware::{BodyStream, MiddlewareChain, ResponseHead, ShortResponse};
use tokio::sync::mpsc;

use body::{StreamBody, run_encoders};
use slot::Chunk;

pub(crate) use body::ResponseBody;
pub(crate) use slot::{HttpResponder, RequestGuard, ResponseSlot, Start};

/// True for HTTP statuses that carry no message body (RFC 9112 §6.3: 1xx, 204, 304).
/// https://www.rfc-editor.org/rfc/rfc9112#section-6.3
#[inline]
pub(crate) fn is_bodiless_status(status: u16) -> bool {
    status < 200 || matches!(status, 204 | 304)
}

#[inline]
fn is_hop_by_hop(name: &HeaderName) -> bool {
    *name == CONTENT_LENGTH
        || *name == TRANSFER_ENCODING
        || *name == CONNECTION
        || *name == UPGRADE
        || *name == TRAILER
        || *name == TE
        || name.as_str() == "keep-alive"
        || name.as_str() == "proxy-connection"
}

/// Strips hop-by-hop headers (RFC 9110 §7.6.1) plus any header names listed in `Connection`.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    if headers.is_empty() || !headers.keys().any(is_hop_by_hop) {
        return;
    }
    const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
    const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");
    if headers.contains_key(CONNECTION) {
        let mut nominated: Vec<HeaderName> = Vec::new();
        for val in headers.get_all(CONNECTION) {
            if let Ok(text) = val.to_str() {
                for token in text.split(',') {
                    let token = token.trim();
                    if !token.is_empty()
                        && let Ok(name) = HeaderName::from_bytes(token.as_bytes())
                    {
                        nominated.push(name);
                    }
                }
            }
        }
        for name in nominated {
            headers.remove(&name);
        }
    }
    headers.remove(CONTENT_LENGTH);
    headers.remove(TRANSFER_ENCODING);
    headers.remove(CONNECTION);
    headers.remove(KEEP_ALIVE);
    headers.remove(UPGRADE);
    headers.remove(TRAILER);
    headers.remove(TE);
    headers.remove(PROXY_CONNECTION);
}

/// Builds a standardized host refusal response (`Cache-Control: private, no-store`, `Connection: close`).
pub(crate) fn refusal(status: StatusCode, guard: RequestGuard) -> Response<ResponseBody> {
    guard.mark_done();
    let mut res = Response::new(ResponseBody::Empty { _guard: guard });
    *res.status_mut() = status;
    let hdrs = res.headers_mut();
    hdrs.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    hdrs.insert(CONNECTION, HeaderValue::from_static("close"));
    res
}

enum ResponseSource {
    Complete {
        body: Bytes,
        truncated: bool,
    },
    PhpStream {
        rx: mpsc::Receiver<Chunk>,
    },
    MiddlewareStream {
        first: Bytes,
        rest: Box<dyn BodyStream>,
    },
}

struct ResponseSpec {
    head: ResponseHead,
    /// Declared uncompressed length before response-head middleware runs.
    declared_len: Option<u64>,
    bodiless: bool,
    outer_depth: Option<usize>,
    source: ResponseSource,
}

/// Single HTTP response normalizer shared by Middleware short-circuits, CGI, and Dispatcher responses.
fn build_response(
    chain: &MiddlewareChain,
    is_head: bool,
    closing: bool,
    extensions: &Extensions,
    spec: ResponseSpec,
    mut guard: RequestGuard,
) -> Response<ResponseBody> {
    let ResponseSpec {
        mut head,
        declared_len,
        bodiless,
        outer_depth,
        source,
    } = spec;

    let truncated_complete = matches!(
        source,
        ResponseSource::Complete {
            truncated: true,
            ..
        }
    );

    if head.status < 200 {
        guard.arm_complete();
        let mut res = Response::new(ResponseBody::Empty { _guard: guard });
        *res.status_mut() = StatusCode::BAD_GATEWAY;
        if closing || truncated_complete {
            res.headers_mut()
                .insert(CONNECTION, HeaderValue::from_static("close"));
        }
        return res;
    }

    let bodiless = bodiless || is_head || is_bodiless_status(head.status);
    head.known_len = if bodiless {
        declared_len
    } else {
        match &source {
            ResponseSource::Complete {
                truncated: true, ..
            } => None,
            ResponseSource::Complete { body, .. } => declared_len.or(Some(body.len() as u64)),
            ResponseSource::PhpStream { .. } | ResponseSource::MiddlewareStream { .. } => {
                declared_len
            }
        }
    };

    let mut encoders = chain.run_response_head(outer_depth, is_head, extensions, &mut head);
    let bodiless = bodiless || is_bodiless_status(head.status);
    let status = StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = head.headers;
    strip_hop_by_hop(&mut headers);
    if closing || (truncated_complete && bodiless) {
        headers.insert(CONNECTION, HeaderValue::from_static("close"));
    }

    let resp_body = if bodiless {
        if is_head
            && !matches!(head.status, 204 | 304)
            && encoders.is_empty()
            && let Some(cl) = head.known_len
        {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(cl));
        }
        guard.arm_complete();
        ResponseBody::Empty { _guard: guard }
    } else {
        match source {
            ResponseSource::Complete { body, truncated } => {
                let under_run =
                    !truncated && declared_len.is_some_and(|cl| (body.len() as u64) < cl);
                if truncated || under_run {
                    let wire = if encoders.is_empty() {
                        body
                    } else {
                        match run_encoders(&mut encoders, &body, false) {
                            Ok(b) => b,
                            Err(_) => return refusal(StatusCode::INTERNAL_SERVER_ERROR, guard),
                        }
                    };
                    let hint_len = (under_run && encoders.is_empty())
                        .then_some(declared_len)
                        .flatten();
                    ResponseBody::Stream(Box::new(StreamBody::truncated(
                        (!wire.is_empty()).then_some(wire),
                        hint_len,
                        guard,
                    )))
                } else {
                    let wire = if encoders.is_empty() {
                        body
                    } else {
                        match run_encoders(&mut encoders, &body, true) {
                            Ok(b) => b,
                            Err(_) => return refusal(StatusCode::INTERNAL_SERVER_ERROR, guard),
                        }
                    };
                    guard.arm_complete();
                    if wire.is_empty() {
                        ResponseBody::Empty { _guard: guard }
                    } else {
                        ResponseBody::Once {
                            data: Some(wire),
                            _guard: guard,
                        }
                    }
                }
            }
            ResponseSource::PhpStream { rx } => {
                if encoders.is_empty() && head.known_len == Some(0) {
                    guard.arm_complete();
                    ResponseBody::Empty { _guard: guard }
                } else {
                    ResponseBody::Stream(Box::new(StreamBody::php(
                        rx,
                        encoders,
                        declared_len,
                        guard,
                    )))
                }
            }
            ResponseSource::MiddlewareStream { first, rest } => ResponseBody::Stream(Box::new(
                StreamBody::middleware(first, rest, encoders, declared_len, guard),
            )),
        }
    };

    let mut res = Response::new(resp_body);
    *res.status_mut() = status;
    *res.headers_mut() = headers;
    res
}

/// Converts a Phase-1 middleware `ShortResponse` into a `hyper` response, running outer Phase-2 hooks.
pub(crate) fn build_short_response(
    chain: &MiddlewareChain,
    outer_depth: usize,
    is_head: bool,
    closing: bool,
    extensions: &Extensions,
    short: ShortResponse,
    guard: RequestGuard,
) -> Response<ResponseBody> {
    let declared_len = short.head.known_len;
    let source = match short.rest {
        Some(rest) => ResponseSource::MiddlewareStream {
            first: short.body,
            rest,
        },
        None => ResponseSource::Complete {
            body: short.body,
            truncated: false,
        },
    };
    build_response(
        chain,
        is_head,
        closing,
        extensions,
        ResponseSpec {
            head: short.head,
            declared_len,
            bodiless: false,
            outer_depth: Some(outer_depth),
            source,
        },
        guard,
    )
}

/// Converts a PHP `Start` message into a `hyper` `Response<ResponseBody>`.
pub(crate) fn build_php_response(
    chain: &MiddlewareChain,
    is_head: bool,
    closing: bool,
    extensions: &Extensions,
    start: Start,
    guard: RequestGuard,
) -> Response<ResponseBody> {
    let spec = match start {
        Start::Error(code) => {
            let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return refusal(status, guard);
        }
        Start::Complete {
            head,
            body,
            content_length,
            bodiless,
            truncated,
        } => ResponseSpec {
            head,
            declared_len: content_length,
            bodiless,
            outer_depth: None,
            source: ResponseSource::Complete { body, truncated },
        },
        Start::Stream {
            head,
            content_length,
            rx,
        } => ResponseSpec {
            head,
            declared_len: content_length,
            bodiless: false,
            outer_depth: None,
            source: ResponseSource::PhpStream { rx },
        },
    };
    build_response(chain, is_head, closing, extensions, spec, guard)
}
