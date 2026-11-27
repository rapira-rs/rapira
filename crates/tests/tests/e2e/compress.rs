use std::io::Read;
use std::time::Duration;

use rapira_config::Mode;

use crate::harness::{Conn, Spawn, diagnostics, fixture_path, http_get_raw, scratch_dir};

const T: Duration = Duration::from_secs(10);

fn split_raw(raw: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head terminator");
    let head_str = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head_str.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status");
    let fields: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        })
        .collect();
    let raw_body = &raw[split + 4..];
    let is_chunked = fields
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    let body = if is_chunked {
        dechunk(raw_body)
    } else {
        raw_body.to_vec()
    };
    (status, fields, body)
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let crlf = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .expect("chunk size crlf");
        let size = usize::from_str_radix(std::str::from_utf8(&raw[..crlf]).unwrap().trim(), 16)
            .expect("hex chunk size");
        raw = &raw[crlf + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
    out
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
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
        let raw = http_get_raw(srv.addr, case.target, &headers, T)
            .unwrap_or_else(|e| panic!("{}: {e}\n{}", case.name, diagnostics(&srv)));
        let (status, fields, body) = split_raw(&raw);
        assert_eq!(status, 200, "{}\n{}", case.name, diagnostics(&srv));
        assert_eq!(
            field(&fields, "content-encoding"),
            case.want_encoding,
            "{}: fields={fields:?}",
            case.name
        );
        if let Some(etag) = case.want_etag {
            assert_eq!(
                field(&fields, "etag"),
                Some(etag),
                "{}: fields={fields:?}",
                case.name
            );
        }
        let decoded = match case.want_encoding {
            Some("gzip") => gunzip(&body),
            Some("zstd") => unzstd(&body),
            _ => body,
        };
        assert_eq!(
            String::from_utf8_lossy(&decoded),
            case.want_body,
            "{}",
            case.name
        );
        if matches!(case.want_encoding, Some("gzip" | "zstd")) {
            let vary = field(&fields, "vary").unwrap_or_default();
            assert!(
                vary.to_ascii_lowercase().contains("accept-encoding"),
                "{}: missing Vary: Accept-Encoding in {fields:?}",
                case.name
            );
        }
    }

    // sendFile() transparently routes through BodyEncoder when compression is active.
    let file_path = srv.dir.join("http/large.txt");
    let file_contents = "sendfile-compressed-line\n".repeat(400);
    std::fs::write(&file_path, &file_contents).expect("write sendfile fixture");
    let raw = http_get_raw(
        srv.addr,
        "/?case=sendfile",
        &[
            ("Accept-Encoding", "zstd"),
            ("X-Path", &file_path.display().to_string()),
        ],
        T,
    )
    .expect("sendfile compressed");
    let (status, fields, body) = split_raw(&raw);
    assert_eq!(status, 200);
    assert_eq!(field(&fields, "content-encoding"), Some("zstd"));
    assert_eq!(String::from_utf8_lossy(&unzstd(&body)), file_contents);
}

/// Onion order applies outer `compress` to inner `static` hits, while `static` outer to `compress` serves static hits uncompressed.
#[test]
fn compress_and_static_follow_config_onion_order() {
    let root = scratch_dir();
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

        let static_raw = http_get_raw(srv.addr, "/app.css", &[("Accept-Encoding", "gzip")], T)
            .expect("GET /app.css");
        let (status, fields, body) = split_raw(&static_raw);
        assert_eq!(status, 200, "{}", case.name);
        assert_eq!(
            field(&fields, "content-encoding"),
            case.static_encoded,
            "{}: static fields={fields:?}",
            case.name
        );
        let decoded = match case.static_encoded {
            Some("gzip") => gunzip(&body),
            _ => body,
        };
        assert_eq!(String::from_utf8_lossy(&decoded), css, "{}", case.name);

        let php_raw = http_get_raw(srv.addr, "/?case=text", &[("Accept-Encoding", "gzip")], T)
            .expect("GET /?case=text");
        let (status, fields, body) = split_raw(&php_raw);
        assert_eq!(status, 200, "{}", case.name);
        assert_eq!(
            field(&fields, "content-encoding"),
            case.php_encoded,
            "{}: php fields={fields:?}",
            case.name
        );
        let decoded = gunzip(&body);
        assert_eq!(
            String::from_utf8_lossy(&decoded),
            "hello rapira compression ".repeat(100),
            "{}",
            case.name
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// HEAD responses apply compression headers using the representation length and omit the uncompressed Content-Length.
#[test]
fn compress_middleware_handles_head_without_uncompressed_content_length() {
    let root = scratch_dir();
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
        let mut conn = Conn::open(srv.addr, T).expect("connect");
        let mut req = format!(
            "HEAD {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n",
            case.target
        );
        if let Some(ae) = case.accept {
            req.push_str(&format!("Accept-Encoding: {ae}\r\n"));
        }
        req.push_str("\r\n");
        conn.send(req.as_bytes()).expect("send HEAD");
        let head = conn
            .read_head(T)
            .unwrap_or_else(|e| panic!("{}: {e}\n{}", case.name, diagnostics(&srv)));
        let rest = conn.read_remaining(T).expect("read EOF");
        assert_eq!(head.status, 200, "{}", case.name);
        assert!(rest.is_empty(), "{}: HEAD body must be empty", case.name);
        assert_eq!(
            field(&head.fields, "content-encoding"),
            case.want_encoding,
            "{}: fields={:?}",
            case.name,
            head.fields
        );
        assert_eq!(
            field(&head.fields, "content-length"),
            case.want_content_length,
            "{}: fields={:?}",
            case.name,
            head.fields
        );
        if let Some(etag) = case.want_etag {
            assert_eq!(
                field(&head.fields, "etag"),
                Some(etag),
                "{}: fields={:?}",
                case.name,
                head.fields
            );
        }
        if case.want_encoding.is_some() {
            let vary = field(&head.fields, "vary").unwrap_or_default();
            assert!(
                vary.to_ascii_lowercase().contains("accept-encoding"),
                "{}: missing Vary: Accept-Encoding in {:?}",
                case.name,
                head.fields
            );
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}
