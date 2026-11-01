//! The response fields that reach the wire and the refusal responses.

use http::header::{
    CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use http_body_util::BodyExt;

use crate::middleware::{Body, BoxError, Response};

pub(crate) fn empty_body() -> Body {
    http_body_util::Empty::<hyper::body::Bytes>::new()
        .map_err(BoxError::from)
        .boxed_unsync()
}

/// hyper closes the connection after a refusal with an unread body but announces nothing, and an ingress with a catch-all cache rule would store the refusal.
pub(crate) fn error_response(status: http::StatusCode) -> Response {
    let mut res = http::Response::new(empty_body());
    *res.status_mut() = status;
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    res.headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    res
}

/// Framing and hop-by-hop fields: hyper frames the response and owns the connection.
static HOP_BY_HOP: [HeaderName; 8] = [
    CONTENT_LENGTH,
    TRANSFER_ENCODING,
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    UPGRADE,
    TRAILER,
    TE,
    HeaderName::from_static("proxy-connection"),
];

/// Removes the hop-by-hop fields and the fields that a Connection value names, then sets the declared content-length. https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1
pub(crate) fn response_headers(mut headers: HeaderMap, content_length: Option<u64>) -> HeaderMap {
    // Connection is in HOP_BY_HOP, so a map without a hop-by-hop key has nothing to remove.
    if headers.keys().any(|k| HOP_BY_HOP.contains(k)) {
        let mut named: Vec<HeaderName> = Vec::new();
        for value in headers.get_all(CONNECTION) {
            for token in value.as_bytes().split(|&b| b == b',') {
                if let Ok(name) = HeaderName::from_bytes(token.trim_ascii()) {
                    named.push(name);
                }
            }
        }
        for name in named.iter().chain(&HOP_BY_HOP) {
            headers.remove(name);
        }
    }
    if let Some(n) = content_length {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(n));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn response_headers_strip_the_framing_and_set_the_length() {
        struct Case {
            name: &'static str,
            input: &'static [(&'static str, &'static str)],
            content_length: Option<u64>,
            want: &'static [(&'static str, &'static str)],
        }
        let cases = [
            Case {
                name: "only content-type, content-length inserted",
                input: &[("Content-Type", "text/plain")],
                content_length: Some(5),
                want: &[("content-type", "text/plain"), ("content-length", "5")],
            },
            Case {
                name: "only content-type, no declared length",
                input: &[("Content-Type", "text/plain")],
                content_length: None,
                want: &[("content-type", "text/plain")],
            },
            Case {
                name: "empty map, content-length inserted",
                input: &[],
                content_length: Some(0),
                want: &[("content-length", "0")],
            },
            Case {
                name: "php framing fields never reach the wire",
                input: &[("Content-Length", "999"), ("Transfer-Encoding", "chunked")],
                content_length: Some(4),
                want: &[("content-length", "4")],
            },
            Case {
                name: "a connection value names fields to drop but cannot strip the framing",
                input: &[
                    ("cOnNeCtIoN", "  Content-Length, ,X-Drop\t,  "),
                    ("X-DROP", "1"),
                    ("X-Keep", "2"),
                    ("PROXY-CONNECTION", "legacy"),
                    ("Content-Type", "text/plain"),
                ],
                content_length: Some(7),
                want: &[
                    ("x-keep", "2"),
                    ("content-type", "text/plain"),
                    ("content-length", "7"),
                ],
            },
        ];
        for c in &cases {
            let map = response_headers(hdrs(c.input), c.content_length);
            assert_eq!(map, hdrs(c.want), "{}", c.name);
        }
    }

    #[test]
    fn error_response_is_minimal_and_closes() {
        let res = error_response(http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(res.status(), 413);
        assert_eq!(res.headers().get("connection").unwrap(), "close");
        assert_eq!(
            res.headers().get("cache-control").unwrap(),
            "private, no-store"
        );
    }
}
