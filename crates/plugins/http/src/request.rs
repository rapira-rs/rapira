//! Builds the SAPI Request from the HTTP request and the Peer.

use rapira_sapi::Request;
use rapira_sapi::types::Chunks;

use crate::Config;
use crate::middleware::Peer;

/// The body stays raw: the handler parses multipart.
pub fn build(
    parts: http::request::Parts,
    authority: Option<Vec<u8>>,
    body: Chunks,
    peer: Peer,
    cfg: &Config,
) -> Request {
    // hyper's HTTP/1 server parses only HTTP/1.0 and HTTP/1.1; HTTP/2 is planned.
    let protocol = match parts.version {
        http::Version::HTTP_10 => "HTTP/1.0",
        http::Version::HTTP_2 => "HTTP/2.0",
        _ => "HTTP/1.1",
    };
    Request {
        method: parts.method.as_str().to_owned(),
        // Origin-form view for every target form. Display restores the leading slash
        // that an authority-only absolute-form omits ("http://h?q" -> "/?q"). It keeps
        // "*" unchanged.
        // https://github.com/hyperium/http/blob/v1.5.0/src/uri/path.rs#L277-L291
        uri: parts
            .uri
            .path_and_query()
            .map(|pq| pq.to_string())
            .unwrap_or_else(|| "/".to_owned()),
        // Reconstructed request-target for the absolute and authority forms. hyper hands out only the parsed Uri, not the raw bytes.
        // For the origin and asterisk forms the target equals uri, which PHP uses when this is None.
        target: parts
            .uri
            .authority()
            .map(|_| parts.uri.to_string().into_bytes()),
        authority,
        https: peer.https,
        protocol,
        remote: peer.remote,
        server: peer.server,
        server_name: cfg.server_name,
        server_port: cfg.server_port,
        received_at: peer.received_at,
        body,
        headers: parts.headers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_sapi::Addr;

    /// build() reads only the server name and port.
    fn config() -> Config {
        Config {
            listen: rapira_net::ListenAddr::Tcp(([127, 0, 0, 1], 8000).into()),
            server_name: "localhost",
            server_port: 8000,
            max_body_size: 0,
            superglobals: true,
            write_timeout: std::time::Duration::ZERO,
            keepalive_timeout: std::time::Duration::ZERO,
            middleware: Vec::new(),
            uploads: None,
            sendfile_root: std::path::PathBuf::new(),
        }
    }

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
        let (parts, ()) = req.into_parts();
        let built = build(
            parts,
            Some(b"e2e".to_vec()),
            Chunks::default(),
            peer(),
            &config(),
        );
        let probes: Vec<_> = built.headers.get_all("x-probe").iter().collect();
        assert_eq!(probes, ["one", "two"]);
        assert_eq!(built.protocol, "HTTP/1.1");
        assert_eq!(built.authority.as_deref(), Some(&b"e2e"[..]));
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
            let req = http::Request::builder()
                .method(case.method)
                .uri(case.uri)
                .body(())
                .unwrap();
            let (parts, ()) = req.into_parts();
            let built = build(parts, None, Chunks::default(), peer(), &config());
            assert_eq!(built.uri, case.want_uri, "{}", case.name);
            assert_eq!(built.target.as_deref(), case.want_target, "{}", case.name);
        }
    }
}
