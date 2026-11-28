use std::io::Read;
use std::net::SocketAddr;

use http::Method;
use http::header::{HeaderName, HeaderValue};
use rapira_config::Mode;
use tests::wire::submit;
use tests::{Resp, drain_resp, req};

use crate::harness::{Spawn, diagnostics, fixture_path, scratch_dir};

fn send(addr: SocketAddr, method: Method, target: &str, headers: &[(&str, &str)]) -> Resp {
    let mut r = req(target);
    *r.method_mut() = method;
    for &(name, value) in headers {
        r.headers_mut().append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    drain_resp(submit(addr, r).expect("submit request"))
}

fn gunzip(data: &[u8]) -> Vec<u8> {
    let mut dec = flate2::read::GzDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("valid gzip stream");
    out
}

fn unzstd(data: &[u8]) -> Vec<u8> {
    zstd::stream::decode_all(data).expect("valid zstd stream")
}

/// Phase-2 and Phase-3 compression runs inline for one-shot, streaming, and sendFile PHP responses.
#[test]
fn compress_middleware_negotiates_and_encodes_php_responses() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path("compress/worker.php"))
        .http_extra("middleware = [\"compress\"]\n[http.compress]\nmin_size = 64")
        .spawn();
    let expected_text = "hello rapira compression ".repeat(100);

    struct Case {
        name: &'static str,
        target: &'static str,
        accept: Option<&'static str>,
        want_encoding: Option<&'static str>,
        want_etag: Option<&'static str>,
        want_body: String,
    }
    let cases = [
        Case {
            name: "zstd preferred over gzip on equal quality",
            target: "/?case=text",
            accept: Some("gzip, zstd"),
            want_encoding: Some("zstd"),
            want_etag: Some("W/\"v1\""),
            want_body: expected_text.clone(),
        },
        Case {
            name: "gzip selected when client requests gzip only",
            target: "/?case=text",
            accept: Some("gzip"),
            want_encoding: Some("gzip"),
            want_etag: Some("W/\"v1\""),
            want_body: expected_text.clone(),
        },
        Case {
            name: "quality weights select gzip over lower-q zstd",
            target: "/?case=text",
            accept: Some("zstd;q=0.5, gzip;q=0.9"),
            want_encoding: Some("gzip"),
            want_etag: Some("W/\"v1\""),
            want_body: expected_text.clone(),
        },
        Case {
            name: "q=0 disables encoding",
            target: "/?case=text",
            accept: Some("gzip;q=0, zstd;q=0"),
            want_encoding: None,
            want_etag: Some("\"v1\""),
            want_body: expected_text.clone(),
        },
        Case {
            name: "response below min_size stays uncompressed",
            target: "/?case=small",
            accept: Some("gzip, zstd"),
            want_encoding: None,
            want_etag: None,
            want_body: "tiny".to_owned(),
        },
        Case {
            name: "incompressible MIME stays uncompressed",
            target: "/?case=binary",
            accept: Some("gzip, zstd"),
            want_encoding: None,
            want_etag: None,
            want_body: "B".repeat(4096),
        },
        Case {
            name: "pre-encoded response keeps existing content-encoding",
            target: "/?case=preencoded",
            accept: Some("gzip, zstd"),
            want_encoding: Some("br"),
            want_etag: None,
            want_body: "P".repeat(4096),
        },
        Case {
            name: "no-transform cache-control skips compression",
            target: "/?case=notransform",
            accept: Some("gzip, zstd"),
            want_encoding: None,
            want_etag: None,
            want_body: "N".repeat(4096),
        },
        Case {
            name: "streaming chunked PHP response compresses incrementally",
            target: "/?case=stream",
            accept: Some("gzip"),
            want_encoding: Some("gzip"),
            want_etag: None,
            want_body: format!("{}{}", "alpha-".repeat(200), "omega-".repeat(200)),
        },
    ];

    for case in cases {
        let headers: Vec<(&str, &str)> = case
            .accept
            .map(|v| vec![("Accept-Encoding", v)])
            .unwrap_or_default();
        let resp = send(srv.addr, Method::GET, case.target, &headers);
        assert_eq!(resp.status(), 200, "{}\n{}", case.name, diagnostics(&srv));
        assert_eq!(
            resp.header("content-encoding").as_deref(),
            case.want_encoding,
            "{}: head={:?}",
            case.name,
            resp.head
        );
        if let Some(etag) = case.want_etag {
            assert_eq!(
                resp.header("etag").as_deref(),
                Some(etag),
                "{}: head={:?}",
                case.name,
                resp.head
            );
        }
        if matches!(case.want_encoding, Some("gzip" | "zstd")) {
            let vary = resp.header("vary").unwrap_or_default();
            assert!(
                vary.to_ascii_lowercase().contains("accept-encoding"),
                "{}: missing Vary: Accept-Encoding in {:?}",
                case.name,
                resp.head
            );
        }
        let decoded = match case.want_encoding {
            Some("gzip") => gunzip(&resp.body),
            Some("zstd") => unzstd(&resp.body),
            _ => resp.body,
        };
        assert_eq!(
            String::from_utf8_lossy(&decoded),
            case.want_body,
            "{}",
            case.name
        );
    }

    // sendFile() transparently routes through BodyEncoder when compression is active.
    let file_path = srv.dir.join("http/large.txt");
    let file_contents = "sendfile-compressed-line\n".repeat(400);
    std::fs::write(&file_path, &file_contents).expect("write sendfile fixture");
    let resp = send(
        srv.addr,
        Method::GET,
        "/?case=sendfile",
        &[
            ("Accept-Encoding", "zstd"),
            ("X-Path", &file_path.display().to_string()),
        ],
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.header("content-encoding").as_deref(), Some("zstd"));
    assert_eq!(resp.header("vary").as_deref(), Some("Accept-Encoding"));
    assert_eq!(String::from_utf8_lossy(&unzstd(&resp.body)), file_contents);
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Onion order applies outer `compress` to inner `static` hits, while `static` outer to `compress` serves static hits uncompressed.
#[test]
fn compress_and_static_follow_config_onion_order() {
    let root = scratch_dir();
    let _cleanup = TempDir(root.clone());
    let css = "body { color: red; }\n".repeat(200);
    std::fs::write(root.join("app.css"), &css).expect("write app.css");

    struct Case {
        name: &'static str,
        order: &'static str,
        static_encoded: Option<&'static str>,
        php_encoded: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "compress outside static compresses both static hits and PHP misses",
            order: "[\"compress\", \"static\"]",
            static_encoded: Some("gzip"),
            php_encoded: Some("gzip"),
        },
        Case {
            name: "static outside compress serves static hits raw and compresses PHP misses",
            order: "[\"static\", \"compress\"]",
            static_encoded: None,
            php_encoded: Some("gzip"),
        },
    ];

    for case in cases {
        let srv = Spawn::http(Mode::Dispatcher, fixture_path("compress/worker.php"))
            .http_extra(&format!(
                "middleware = {}\n[http.compress]\nmin_size = 64\n[http.static]\nroot = \"{}\"",
                case.order,
                root.display()
            ))
            .spawn();

        let static_resp = send(
            srv.addr,
            Method::GET,
            "/app.css",
            &[("Accept-Encoding", "gzip")],
        );
        assert_eq!(static_resp.status(), 200, "{}", case.name);
        assert_eq!(
            static_resp.header("content-encoding").as_deref(),
            case.static_encoded,
            "{}: static head={:?}",
            case.name,
            static_resp.head
        );
        let decoded = match case.static_encoded {
            Some("gzip") => gunzip(&static_resp.body),
            _ => static_resp.body,
        };
        assert_eq!(String::from_utf8_lossy(&decoded), css, "{}", case.name);

        let php_resp = send(
            srv.addr,
            Method::GET,
            "/?case=text",
            &[("Accept-Encoding", "gzip")],
        );
        assert_eq!(php_resp.status(), 200, "{}", case.name);
        assert_eq!(
            php_resp.header("content-encoding").as_deref(),
            case.php_encoded,
            "{}: php head={:?}",
            case.name,
            php_resp.head
        );
        let decoded = match case.php_encoded {
            Some("gzip") => gunzip(&php_resp.body),
            Some("zstd") => unzstd(&php_resp.body),
            _ => php_resp.body,
        };
        assert_eq!(
            String::from_utf8_lossy(&decoded),
            "hello rapira compression ".repeat(100),
            "{}",
            case.name
        );
    }
}

/// HEAD responses apply compression headers using the representation length and omit the uncompressed Content-Length.
#[test]
fn compress_middleware_handles_head_without_uncompressed_content_length() {
    let root = scratch_dir();
    let _cleanup = TempDir(root.clone());
    let css = "body { color: red; }\n".repeat(200);
    std::fs::write(root.join("app.css"), &css).expect("write app.css");

    let srv = Spawn::http(Mode::Dispatcher, fixture_path("compress/worker.php"))
        .http_extra(&format!(
            "middleware = [\"compress\", \"static\"]\n[http.compress]\nmin_size = 64\n[http.static]\nroot = \"{}\"",
            root.display()
        ))
        .spawn();

    struct Case {
        name: &'static str,
        target: &'static str,
        accept: Option<&'static str>,
        want_encoding: Option<&'static str>,
        want_content_length: Option<&'static str>,
        want_etag: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "HEAD on PHP response with gzip removes uncompressed content-length",
            target: "/?case=head_cl",
            accept: Some("gzip"),
            want_encoding: Some("gzip"),
            want_content_length: None,
            want_etag: Some("W/\"v1\""),
        },
        Case {
            name: "HEAD on static file with zstd uses file length and omits uncompressed content-length",
            target: "/app.css",
            accept: Some("zstd"),
            want_encoding: Some("zstd"),
            want_content_length: None,
            want_etag: None,
        },
        Case {
            name: "HEAD without Accept-Encoding keeps uncompressed content-length",
            target: "/?case=head_cl",
            accept: None,
            want_encoding: None,
            want_content_length: Some("2500"),
            want_etag: Some("\"v1\""),
        },
    ];

    for case in cases {
        let headers: Vec<(&str, &str)> = case
            .accept
            .map(|v| vec![("Accept-Encoding", v)])
            .unwrap_or_default();
        let resp = send(srv.addr, Method::HEAD, case.target, &headers);
        assert_eq!(resp.status(), 200, "{}\n{}", case.name, diagnostics(&srv));
        assert!(
            resp.body.is_empty(),
            "{}: HEAD body must be empty",
            case.name
        );
        assert_eq!(
            resp.header("content-encoding").as_deref(),
            case.want_encoding,
            "{}: head={:?}",
            case.name,
            resp.head
        );
        assert_eq!(
            resp.header("content-length").as_deref(),
            case.want_content_length,
            "{}: head={:?}",
            case.name,
            resp.head
        );
        if let Some(etag) = case.want_etag {
            assert_eq!(
                resp.header("etag").as_deref(),
                Some(etag),
                "{}: head={:?}",
                case.name,
                resp.head
            );
        }
        if case.want_encoding.is_some() {
            let vary = resp.header("vary").unwrap_or_default();
            assert!(
                vary.to_ascii_lowercase().contains("accept-encoding"),
                "{}: missing Vary: Accept-Encoding in {:?}",
                case.name,
                resp.head
            );
        }
    }
}

/// Conditional 304 responses for compressed static assets keep Vary: Accept-Encoding and match the 200 ETag weakness.
#[test]
fn compress_preserves_vary_and_weak_etag_on_static_304() {
    let root = scratch_dir();
    let _cleanup = TempDir(root.clone());
    let css = "body { color: blue; }\n".repeat(200);
    std::fs::write(root.join("app.css"), &css).expect("write app.css");
    let png = vec![0x89u8; 4096];
    std::fs::write(root.join("logo.png"), &png).expect("write logo.png");

    let srv = Spawn::http(Mode::Dispatcher, fixture_path("compress/worker.php"))
        .http_extra(&format!(
            "middleware = [\"compress\", \"static\"]\n[http.compress]\nmin_size = 64\n[http.static]\nroot = \"{}\"",
            root.display()
        ))
        .spawn();

    let css_resp = send(
        srv.addr,
        Method::GET,
        "/app.css",
        &[("Accept-Encoding", "gzip")],
    );
    assert_eq!(css_resp.status(), 200);
    let css_weak_etag = css_resp.header("etag").expect("css etag");
    assert!(
        css_weak_etag.starts_with("W/\""),
        "css 200 etag={css_weak_etag}"
    );
    let css_strong_etag = css_weak_etag.strip_prefix("W/").unwrap().to_owned();
    let css_last_mod = css_resp.header("last-modified").expect("css last-modified");

    let png_resp = send(
        srv.addr,
        Method::GET,
        "/logo.png",
        &[("Accept-Encoding", "gzip")],
    );
    assert_eq!(png_resp.status(), 200);
    let png_strong_etag = png_resp.header("etag").expect("png etag");
    assert!(
        png_strong_etag.starts_with('"'),
        "png 200 etag={png_strong_etag}"
    );

    struct Case {
        name: &'static str,
        target: &'static str,
        cond_header: &'static str,
        cond_value: String,
        accept: Option<&'static str>,
        want_etag: String,
        want_vary: bool,
    }
    let cases = [
        Case {
            name: "304 css via If-None-Match with gzip weakens etag and keeps Vary",
            target: "/app.css",
            cond_header: "If-None-Match",
            cond_value: css_weak_etag.clone(),
            accept: Some("gzip"),
            want_etag: css_weak_etag.clone(),
            want_vary: true,
        },
        Case {
            name: "304 css via If-Modified-Since with gzip weakens etag and keeps Vary",
            target: "/app.css",
            cond_header: "If-Modified-Since",
            cond_value: css_last_mod,
            accept: Some("gzip"),
            want_etag: css_weak_etag,
            want_vary: true,
        },
        Case {
            name: "304 css without Accept-Encoding keeps strong etag and keeps Vary",
            target: "/app.css",
            cond_header: "If-None-Match",
            cond_value: css_strong_etag.clone(),
            accept: None,
            want_etag: css_strong_etag,
            want_vary: true,
        },
        Case {
            name: "304 incompressible png with gzip keeps strong etag without Vary",
            target: "/logo.png",
            cond_header: "If-None-Match",
            cond_value: png_strong_etag.clone(),
            accept: Some("gzip"),
            want_etag: png_strong_etag,
            want_vary: false,
        },
    ];

    for case in cases {
        let mut headers = vec![(case.cond_header, case.cond_value.as_str())];
        if let Some(ae) = case.accept {
            headers.push(("Accept-Encoding", ae));
        }
        let resp = send(srv.addr, Method::GET, case.target, &headers);
        assert_eq!(resp.status(), 304, "{}: head={:?}", case.name, resp.head);
        assert!(
            resp.body.is_empty(),
            "{}: 304 body must be empty",
            case.name
        );
        assert_eq!(
            resp.header("content-encoding"),
            None,
            "{}: 304 must not set content-encoding",
            case.name
        );
        assert_eq!(
            resp.header("etag").as_deref(),
            Some(case.want_etag.as_str()),
            "{}: head={:?}",
            case.name,
            resp.head
        );
        let has_vary = resp
            .header("vary")
            .is_some_and(|v| v.to_ascii_lowercase().contains("accept-encoding"));
        assert_eq!(
            has_vary, case.want_vary,
            "{}: head={:?}",
            case.name, resp.head
        );
    }
}
