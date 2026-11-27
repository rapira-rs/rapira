//! Builds the SAPI Request from the parsed HTTP head and Peer.

use std::borrow::Cow;

use http::{Method, Uri, Version};
use rapira_http_middleware::{Peer, PeerAddr, RequestHeaders};
use rapira_sapi::types::{Authority, Chunks, RequestUri};
use rapira_sapi::{Addr, Request};

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

/// The body stays raw, because the handler parses multipart.
pub fn build(
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

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{HeaderName, HeaderValue};
    use rapira_http_middleware::PeerAddr;

    fn peer() -> Peer {
        Peer {
            remote: PeerAddr::Inet(([127, 0, 0, 1], 40000).into()),
            server: PeerAddr::Inet(([127, 0, 0, 1], 8000).into()),
            https: false,
            received_at: 1.5,
        }
    }

    #[test]
    fn headers_keep_the_per_name_wire_order() {
        let mut hdrs = RequestHeaders::new();
        hdrs.append(
            HeaderName::from_static("x-probe"),
            HeaderValue::from_static("one"),
        );
        hdrs.append(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/*"),
        );
        hdrs.append(
            HeaderName::from_static("x-probe"),
            HeaderValue::from_static("two"),
        );
        let built = build(
            Method::GET,
            Uri::from_static("/a/b?x=1"),
            Version::HTTP_11,
            hdrs,
            Authority::Explicit(bytes::Bytes::from_static(b"e2e")),
            Chunks::default(),
            peer(),
        );
        let probes: Vec<&[u8]> = built
            .headers
            .get_all("x-probe")
            .iter()
            .map(HeaderValue::as_bytes)
            .collect();
        assert_eq!(probes, [&b"one"[..], &b"two"[..]]);
        assert_eq!(built.protocol, "HTTP/1.1");
        assert_eq!(built.authority_bytes(), Some(&b"e2e"[..]));
        assert_eq!(built.received_at, 1.5);
    }

    /// PHP gets the origin-form view in `uri`; `target` keeps the absolute form and is None for the other forms. https://www.rfc-editor.org/rfc/rfc9112#section-3.2.2
    #[test]
    fn the_target_forms_map_to_uri_and_target() {
        struct Case {
            name: &'static str,
            method: &'static str,
            uri: &'static str,
            want_uri: &'static str,
            want_target: Option<&'static [u8]>,
        }
        let cases = [
            Case {
                name: "origin-form keeps the uri and has no target",
                method: "GET",
                uri: "/a/b?x=1",
                want_uri: "/a/b?x=1",
                want_target: None,
            },
            Case {
                name: "absolute-form yields an origin-form uri",
                method: "GET",
                uri: "http://h.example/admin?x=1",
                want_uri: "/admin?x=1",
                want_target: Some(b"http://h.example/admin?x=1"),
            },
            Case {
                name: "absolute-form without a path roots the uri",
                method: "GET",
                uri: "http://h.example?x=1",
                want_uri: "/?x=1",
                want_target: Some(b"http://h.example/?x=1"),
            },
            Case {
                name: "asterisk-form passes through",
                method: "OPTIONS",
                uri: "*",
                want_uri: "*",
                want_target: None,
            },
        ];
        for case in cases {
            let built = build(
                Method::from_bytes(case.method.as_bytes()).unwrap(),
                Uri::try_from(case.uri).unwrap(),
                Version::HTTP_11,
                RequestHeaders::new(),
                Authority::None,
                Chunks::default(),
                peer(),
            );
            assert_eq!(built.uri.as_str(), case.want_uri, "{}", case.name);
            assert_eq!(built.target.as_deref(), case.want_target, "{}", case.name);
        }
    }
}
