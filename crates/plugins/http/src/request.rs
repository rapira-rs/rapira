use extension_api::{Peer, Request};

use crate::Config;

pub(crate) fn build(
    parts: &http::request::Parts,
    authority: Option<Vec<u8>>,
    body: Vec<u8>,
    peer: &Peer,
    cfg: &Config,
) -> Request {
    let protocol = match parts.version {
        http::Version::HTTP_11 => "HTTP/1.1".to_owned(),
        http::Version::HTTP_10 => "HTTP/1.0".to_owned(),
        v => format!("{v:?}"),
    };
    Request {
        span: tracing::Span::current(),
        method: parts.method.as_str().to_owned(),
        // Origin-form view for every target form. Display restores the leading slash
        // that an authority-only absolute-form omits ("http://h?q" -> "/?q"). It keeps
        // "*" unchanged.
        // https://docs.rs/http/1/http/uri/struct.PathAndQuery.html
        uri: parts
            .uri
            .path_and_query()
            .map(|pq| pq.to_string())
            .unwrap_or_else(|| "/".to_owned()),
        // Reconstructed request-target. hyper hands out only the parsed Uri, not the raw bytes.
        target: Some(parts.uri.to_string().into_bytes()),
        authority,
        https: peer.https,
        protocol,
        remote: peer.remote.clone(),
        server: peer.server.clone(),
        server_name: cfg.server_name.clone(),
        server_port: cfg.server_port,
        tls: None,
        received_at: Some(peer.received_at),
        headers: parts
            .headers
            .iter()
            .map(|(n, v)| (n.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect(),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_api::Addr;

    fn peer() -> Peer {
        Peer {
            remote: Addr::Inet(([127, 0, 0, 1], 40000).into()),
            server: Addr::Inet(([127, 0, 0, 1], 8000).into()),
            https: false,
            received_at: 1.5,
        }
    }

    /// One FieldLines entry per field line, values in per-name wire order, names lowercase.
    #[test]
    fn headers_arrive_per_line_in_per_name_order() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/a/b?x=1")
            .header("X-Probe", "one")
            .header("Accept", "text/*")
            .header("x-probe", "two")
            .body(())
            .unwrap();
        let (parts, ()) = req.into_parts();
        let built = build(
            &parts,
            Some(b"e2e".to_vec()),
            Vec::new(),
            &peer(),
            &Config::default(),
        );
        let probes: Vec<_> = built
            .headers
            .iter()
            .filter(|(n, _)| n == "x-probe")
            .map(|(_, v)| v.as_slice())
            .collect();
        assert_eq!(probes, [b"one".as_slice(), b"two".as_slice()]);
        assert_eq!(built.uri, "/a/b?x=1");
        assert_eq!(built.target.as_deref(), Some(&b"/a/b?x=1"[..]));
        assert_eq!(built.protocol, "HTTP/1.1");
        assert_eq!(built.authority.as_deref(), Some(&b"e2e"[..]));
        assert_eq!(built.received_at, Some(1.5));
    }

    fn built(uri: &str, method: &str) -> Request {
        let req = http::Request::builder()
            .method(method)
            .uri(uri)
            .body(())
            .unwrap();
        let (parts, ()) = req.into_parts();
        build(&parts, None, Vec::new(), &peer(), &Config::default())
    }

    /// RFC 9112 §3.2.2 absolute-form: PHP gets the origin-form view; the target keeps the full form.
    /// https://www.rfc-editor.org/rfc/rfc9112#section-3.2.2
    #[test]
    fn absolute_form_yields_an_origin_form_uri() {
        let b = built("http://h.example/admin?x=1", "GET");
        assert_eq!(b.uri, "/admin?x=1");
        assert_eq!(
            b.target.as_deref(),
            Some(&b"http://h.example/admin?x=1"[..])
        );
    }

    /// An absolute-form target with no path still yields a rooted uri.
    #[test]
    fn empty_path_absolute_form_roots_the_uri() {
        let b = built("http://h.example?x=1", "GET");
        assert_eq!(b.uri, "/?x=1");
    }

    /// Asterisk-form passes through unchanged (server-wide OPTIONS).
    #[test]
    fn asterisk_form_is_preserved() {
        let b = built("*", "OPTIONS");
        assert_eq!(b.uri, "*");
        assert_eq!(b.target.as_deref(), Some(&b"*"[..]));
    }
}
