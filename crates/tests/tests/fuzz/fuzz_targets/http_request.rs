//! Passes an HTTP/1.x request head through `httparse`, admission checks, request build, and `$uri` of dispatcher mode.
//! The first byte selects the superglobals, scheme, and server address. The rest is wire bytes.

#![no_main]

use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Method, Uri, Version};
use rapira_fuzz::safe_name;
use rapira_http::intake::{build_request, check_request};
use rapira_http::php::uri_abs;
use rapira_http_middleware::{Peer, PeerAddr};
use rapira_sapi::types::Chunks;

const MAX_BODY: usize = 1024;
const SERVER: &str = "127.0.0.1:8000";

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let Some((&flags, wire)) = data.split_first() else {
        return;
    };
    let superglobals = flags & 2 != 0;
    let https = flags & 4 != 0;

    let mut raw_headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut raw_headers);
    let Ok(httparse::Status::Complete(_)) = parsed.parse(wire) else {
        return;
    };
    let Some(method_str) = parsed.method else {
        return;
    };
    let Ok(method) = Method::from_bytes(method_str.as_bytes()) else {
        return;
    };
    let Some(path_str) = parsed.path else {
        return;
    };
    let Ok(uri) = Uri::try_from(path_str) else {
        return;
    };
    let version = match parsed.version {
        Some(0) => Version::HTTP_10,
        Some(1) => Version::HTTP_11,
        _ => return,
    };
    let mut headers = HeaderMap::new();
    for h in parsed.headers.iter() {
        let Ok(name) = HeaderName::from_bytes(h.name.as_bytes()) else {
            return;
        };
        let Ok(val) = HeaderValue::from_bytes(h.value) else {
            return;
        };
        headers.append(name, val);
    }

    let mut req = http::Request::new(());
    *req.method_mut() = method;
    *req.uri_mut() = uri;
    *req.version_mut() = version;
    *req.headers_mut() = headers;
    let (mut parts, ()) = req.into_parts();

    let authority = match check_request(&mut parts, superglobals, MAX_BODY) {
        Ok(admitted) => admitted.authority,
        Err(status) => {
            assert!(matches!(status.as_u16(), 400 | 413));
            return;
        }
    };

    if parts.version == Version::HTTP_11 {
        let resolved = match &authority {
            rapira_sapi::types::Authority::None => None,
            rapira_sapi::types::Authority::HostHeader => parts
                .headers
                .get(http::header::HOST)
                .map(|v| v.as_bytes().trim_ascii()),
            rapira_sapi::types::Authority::Explicit(b) => Some(b.as_ref()),
        };
        assert!(
            resolved.is_some_and(|a| !a.is_empty()),
            "HTTP/1.1 must require non-empty Host"
        );
    }
    if superglobals {
        for name in parts.headers.keys() {
            assert!(safe_name(name), "unsafe CGI header leaked: {name}");
        }
    }

    let local = if flags & 1 == 0 { SERVER } else { "[::1]:8123" };
    let peer = Peer {
        remote: PeerAddr::Inet(([127, 0, 0, 1], 40000).into()),
        server: PeerAddr::Inet(local.parse().unwrap()),
        https,
        received_at: 0.0,
    };
    let built = build_request(
        parts.method,
        parts.uri,
        parts.version,
        parts.headers,
        authority,
        Chunks::default(),
        peer,
    );

    let uri_abs_str = uri_abs(&built);
    let a = built.authority_bytes().unwrap_or(local.as_bytes());
    if http::uri::Authority::try_from(a).is_ok() {
        let u = Uri::try_from(uri_abs_str.as_str())
            .unwrap_or_else(|e| panic!("$uri {uri_abs_str}: {e}"));
        assert_eq!(
            u.scheme_str(),
            Some(if https { "https" } else { "http" }),
            "$uri scheme"
        );
        assert_eq!(
            u.authority().map(|x| x.as_str().as_bytes()),
            Some(a),
            "$uri authority"
        );
        let uri_str = built.uri.as_str();
        let path = if uri_str == "*" { "/" } else { uri_str };
        assert_eq!(
            u.path_and_query().map(|p| p.as_str()),
            Some(path),
            "$uri path"
        );
    }
});
