use extension_api::{HttpResponse, empty_body};
use http::header::{
    CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};

pub(crate) fn error_response(status: http::StatusCode) -> HttpResponse {
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

/// Removes the hop-by-hop fields and the fields that a Connection value names, then sets the declared content-length.
pub(crate) fn response_headers(mut headers: HeaderMap, content_length: Option<u64>) -> HeaderMap {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .flat_map(|v| v.as_bytes().split(|&b| b == b','))
        .filter_map(|token| HeaderName::from_bytes(token.trim_ascii()).ok())
        .collect();
    for name in named.iter().chain(&HOP_BY_HOP) {
        headers.remove(name);
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
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn connection_value_cannot_strip_framing() {
        let map = response_headers(
            hdrs(&[
                ("cOnNeCtIoN", "  Content-Length, ,X-Drop\t,  "),
                ("X-DROP", "1"),
                ("X-Keep", "2"),
                ("PROXY-CONNECTION", "legacy"),
                ("Content-Type", "text/plain"),
            ]),
            Some(7),
        );
        assert_eq!(map.get("content-length").unwrap().as_bytes(), b"7");
        assert!(map.get("x-drop").is_none());
        assert_eq!(map.get("x-keep").unwrap().as_bytes(), b"2");
        assert!(map.get("proxy-connection").is_none());
        assert_eq!(map.get("content-type").unwrap().as_bytes(), b"text/plain");
        assert!(map.get("connection").is_none());
    }

    #[test]
    fn php_framing_headers_never_reach_the_wire() {
        let map = response_headers(
            hdrs(&[("Content-Length", "999"), ("Transfer-Encoding", "chunked")]),
            Some(4),
        );
        assert_eq!(map.get("content-length").unwrap().as_bytes(), b"4");
        assert!(map.get("transfer-encoding").is_none());
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
