//! An HTTP/1 request head through the admission checks, the request build and the `$uri` of the dispatcher mode.
//! The first byte selects the field name policy, the superglobals and the scheme. The rest is the request head.

#![no_main]

use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use http::header::{CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri, Version};
use rapira_fuzz::safe_name;
use rapira_http::check::check_request;
use rapira_http::middleware::Peer;
use rapira_http::php::RequestView;
use rapira_http::request::build;
use rapira_http::{Config, UnsafeFieldNames};
use rapira_net::ListenAddr;
use rapira_sapi::Addr;

const MAX_BODY: usize = 1024;
const SERVER: &str = "127.0.0.1:8000";

static CONFIG: LazyLock<Config> = LazyLock::new(|| Config {
    listen: ListenAddr::Tcp(SERVER.parse().unwrap()),
    server_name: "localhost".to_owned(),
    server_port: 8000,
    max_body_size: MAX_BODY,
    unsafe_field_names: UnsafeFieldNames::Drop,
    superglobals: true,
    write_timeout: Duration::ZERO,
    keepalive_timeout: Duration::ZERO,
    middleware: Vec::new(),
    uploads: None,
    sendfile_root: PathBuf::new(),
});

/// RFC 9110 Content-Length = 1*DIGIT: https://www.rfc-editor.org/rfc/rfc9110#section-8.6
fn length(v: &HeaderValue) -> Option<u64> {
    let s = v.to_str().ok()?;
    s.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| s.parse().ok())?
}

/// RFC 9110 Host = uri-host [ ":" port ], https://www.rfc-editor.org/rfc/rfc9110#section-7.2
/// An http URI has a non-empty host, RFC 9110 section 4.2.1. The http crate rejects a percent-encoded reg-name, so this leaves it out.
fn is_host(a: &[u8]) -> bool {
    let (host, port) = match a.iter().rposition(|&b| b == b':') {
        Some(i) if !a[i..].contains(&b']') => (&a[..i], &a[i + 1..]),
        _ => (a, &[][..]),
    };
    let ip_literal = host
        .strip_prefix(b"[")
        .and_then(|h| h.strip_suffix(b"]"))
        .and_then(|h| std::str::from_utf8(h).ok())
        .is_some_and(|h| h.parse::<Ipv6Addr>().is_ok());
    let reg_name = !host.is_empty()
        && host
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=".contains(b));
    port.iter().all(u8::is_ascii_digit) && (ip_literal || reg_name)
}

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let Some((&flags, head)) = data.split_first() else {
        return;
    };
    let policy = if flags & 1 == 0 {
        UnsafeFieldNames::Drop
    } else {
        UnsafeFieldNames::Reject
    };
    let superglobals = flags & 2 != 0;
    let https = flags & 4 != 0;

    // The same parse as the HTTP/1 server of hyper: httparse, then the http types.
    let mut fields = [httparse::EMPTY_HEADER; 100];
    let mut req = httparse::Request::new(&mut fields);
    let Ok(httparse::Status::Complete(_)) = req.parse(head) else {
        return;
    };
    let (Some(method), Some(target), Some(minor)) = (req.method, req.path, req.version) else {
        return;
    };
    let (Ok(method), Ok(uri)) = (Method::from_bytes(method.as_bytes()), Uri::try_from(target))
    else {
        return;
    };
    let version = if minor == 1 {
        Version::HTTP_11
    } else {
        Version::HTTP_10
    };
    let mut sent = HeaderMap::new();
    for f in req.headers.iter() {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(f.name.as_bytes()),
            HeaderValue::from_bytes(f.value),
        ) else {
            return;
        };
        sent.append(name, value);
    }
    // hyper answers 400 to a Content-Length that is not 1*DIGIT and to two different values. Transfer-Encoding changes the framing, which is out of scope here.
    let lengths: Vec<Option<u64>> = sent.get_all(CONTENT_LENGTH).iter().map(length).collect();
    if sent.contains_key(TRANSFER_ENCODING)
        || lengths.iter().any(Option::is_none)
        || lengths.windows(2).any(|w| w[0] != w[1])
    {
        return;
    }
    let declared = lengths.first().copied().flatten();

    let mut parts = http::Request::new(()).into_parts().0;
    parts.method = method.clone();
    parts.uri = uri.clone();
    parts.version = version;
    parts.headers = sent.clone();

    // Each rule that the request breaks allows its status, and a request that breaks no rule must pass.
    let hosts: Vec<&[u8]> = sent
        .get_all(HOST)
        .iter()
        .map(HeaderValue::as_bytes)
        .collect();
    let mut allowed = Vec::new();
    if method == Method::CONNECT {
        allowed.push(501);
    }
    // RFC 9112 section 3.2: https://www.rfc-editor.org/rfc/rfc9112#section-3.2
    // #169: a Host value that is not uri-host [ ":" port ] is accepted, https://github.com/rapira-rs/rapira/issues/169
    if hosts.len() > 1
        || (version == Version::HTTP_11 && hosts.first().is_none_or(|h| h.is_empty()))
    {
        allowed.push(400);
    }
    if policy == UnsafeFieldNames::Reject && !sent.keys().all(safe_name) {
        allowed.push(400);
    }
    if declared.is_some_and(|l| l > MAX_BODY as u64) {
        allowed.push(413);
    }
    let authority = match check_request(&mut parts, policy, superglobals, MAX_BODY) {
        Ok(authority) => {
            assert!(allowed.is_empty(), "accepted, but allowed {allowed:?}");
            authority
        }
        Err(r) => {
            assert!(
                allowed.contains(&r.status.as_u16()),
                "{r}, but allowed {allowed:?}"
            );
            return;
        }
    };

    // RFC 9112 section 3.2.2: the authority is the target authority without userinfo, https://www.rfc-editor.org/rfc/rfc9112#section-3.2.2
    let want_authority = match uri.authority() {
        Some(a) => Some(
            a.as_str()
                .rsplit_once('@')
                .map_or(a.as_str(), |(_, hp)| hp)
                .as_bytes(),
        ),
        None => hosts.first().copied().filter(|h| !h.is_empty()),
    };
    assert_eq!(authority.as_deref(), want_authority, "authority");
    // #193: an absolute-form target rewrites the Host field of Request::$headers, https://github.com/rapira-rs/rapira/issues/193
    let keep_all = policy == UnsafeFieldNames::Drop && !superglobals;
    for name in sent.keys() {
        assert_eq!(
            parts.headers.contains_key(name),
            keep_all || safe_name(name),
            "field {name}"
        );
    }

    let peer = Peer {
        remote: Addr::Inet(([127, 0, 0, 1], 40000).into()),
        server: Addr::Inet(SERVER.parse().unwrap()),
        https,
        received_at: 0.0,
    };
    let built = build(&mut parts, authority.clone(), Vec::new(), peer, &CONFIG);
    // #183: http::Uri drops a fragment, https://github.com/rapira-rs/rapira/issues/183
    let raw = target.split('#').next().unwrap_or_default();
    let (want_uri, want_target) = if raw == "*" || raw.starts_with('/') {
        (raw.to_owned(), None)
    } else if let Some(scheme) = uri.scheme_str() {
        let rest = &raw[scheme.len() + 3..];
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        // #183: http::Uri writes an empty path as "/" and the http and https schemes in lower case, https://github.com/rapira-rs/rapira/issues/183
        let path = if rest[end..].starts_with('/') {
            rest[end..].to_owned()
        } else {
            format!("/{}", &rest[end..])
        };
        let target = format!("{scheme}://{}{path}", &rest[..end]);
        (path, Some(target))
    } else {
        ("/".to_owned(), Some(raw.to_owned()))
    };
    assert_eq!(built.uri, want_uri, "uri");
    assert_eq!(
        built.target.as_deref(),
        want_target.as_deref().map(str::as_bytes),
        "target"
    );

    // Request::$uri: the listener scheme, the authority or else the listener address, and the path. OPTIONS * gives the root.
    let view = RequestView::new(&built);
    let a = authority.as_deref().unwrap_or(SERVER.as_bytes());
    // #169: a Host value that is not uri-host [ ":" port ] goes into $uri as it is, https://github.com/rapira-rs/rapira/issues/169
    if is_host(a) {
        let u = Uri::try_from(view.uri_abs.as_str())
            .unwrap_or_else(|e| panic!("$uri {}: {e}", view.uri_abs));
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
        let path = if built.uri == "*" { "/" } else { &built.uri };
        assert_eq!(
            u.path_and_query().map(|p| p.as_str()),
            Some(path),
            "$uri path"
        );
    }
});
