use rapira_sapi::api::{Peer, Request};

use crate::Config;

/// Moves the header map out of `parts`.
pub(crate) fn build(
    parts: &mut http::request::Parts,
    authority: Option<Vec<u8>>,
    body: Vec<u8>,
    peer: Peer,
    cfg: &Config,
) -> Request {
    let protocol = match parts.version {
        http::Version::HTTP_11 => "HTTP/1.1".to_owned(),
        http::Version::HTTP_10 => "HTTP/1.0".to_owned(),
        v => format!("{v:?}"),
    };
    Request {
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
        // Reconstructed request-target for the absolute and authority forms. hyper hands out only the parsed Uri, not the raw bytes.
        // For the origin and asterisk forms the target equals `uri`, which PHP uses when this is None.
        target: parts
            .uri
            .authority()
            .map(|_| parts.uri.to_string().into_bytes()),
        authority,
        https: peer.https,
        protocol,
        remote: peer.remote,
        server: peer.server,
        server_name: cfg.server_name.clone(),
        server_port: cfg.server_port,
        tls: None,
        received_at: Some(peer.received_at),
        headers: std::mem::take(&mut parts.headers),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_sapi::api::Addr;

    fn peer() -> Peer {
        Peer {
            remote: Addr::Inet(([127, 0, 0, 1], 40000).into()),
            server: Addr::Inet(([127, 0, 0, 1], 8000).into()),
            https: false,
            received_at: 1.5,
        }
    }

    #[test]
    fn headers_keep_the_per_name_wire_order() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/a/b?x=1")
            .header("X-Probe", "one")
            .header("Accept", "text/*")
            .header("x-probe", "two")
            .body(())
            .unwrap();
        let (mut parts, ()) = req.into_parts();
        let built = build(
            &mut parts,
            Some(b"e2e".to_vec()),
            Vec::new(),
            peer(),
            &Config::default(),
        );
        let probes: Vec<_> = built.headers.get_all("x-probe").iter().collect();
        assert_eq!(probes, ["one", "two"]);
        assert_eq!(built.uri, "/a/b?x=1");
        assert_eq!(built.target, None);
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
        let (mut parts, ()) = req.into_parts();
        build(&mut parts, None, Vec::new(), peer(), &Config::default())
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
        assert_eq!(b.target, None);
    }
}
