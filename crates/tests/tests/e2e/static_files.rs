use crate::harness::{
    Conn, Server, Spawn, diagnostics, http_get, http_get_raw, http_post, http_raw_bytes,
    scratch_dir, spawn_boot_failure, spawn_with_http_extra,
};
use rapira_sapi::Mode;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tests::fixture;

const T: Duration = Duration::from_secs(10);

fn static_root(files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = scratch_dir();
    for (name, contents) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("parent dir")).expect("create static dir");
        std::fs::write(path, contents).expect("write static file");
    }
    dir
}

/// Reads one field from the response head. Field names are case-insensitive (RFC 9110
/// section 5.1, https://www.rfc-editor.org/rfc/rfc9110#section-5.1). An absent field gives an
/// empty string, so a caller that compares two fields must first check that the field is
/// present.
fn head_field(raw: &[u8], name: &str) -> String {
    let text = String::from_utf8_lossy(raw);
    let head = text.split("\r\n\r\n").next().unwrap_or_default();
    head.lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default()
}

/// One boot covers the wire behavior: a hit with its headers, a miss into PHP, the source and dotfile guards, and a range.
#[test]
fn static_files_serve_over_the_wire() {
    let root = static_root(&[
        ("app.css", "body{color:red}"),
        ("big.bin", "0123456789"),
        ("index.php", "<?php leak();"),
        (".env", "SECRET=1"),
    ]);
    let srv = spawn_with_http_extra(
        "shared/echo-worker.php",
        1,
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );

    let raw = http_get_raw(srv.addr, "/app.css", &[], T).expect("GET /app.css");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "{text}\n{}",
        diagnostics(&srv)
    );
    let head = text.split("\r\n\r\n").next().expect("head").to_lowercase();
    assert!(head.contains("\r\ncontent-type: text/css\r\n"), "{head}");
    assert!(head.contains("\r\ncontent-length: 15\r\n"), "{head}");
    assert!(head.contains("\r\naccept-ranges: bytes\r\n"), "{head}");
    assert!(text.ends_with("body{color:red}"), "{text}");

    let (code, body) = http_get(srv.addr, "/nope", T).expect("GET /nope");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert!(body.starts_with(b"ok:"), "a miss must reach php");

    let (code, body) =
        http_post(srv.addr, "/nope", b"text/plain", b"payload", T).expect("POST /nope");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert!(body.starts_with(b"ok:"), "a post must reach php");

    for path in ["/index.php", "/.env"] {
        let (_, body) = http_get(srv.addr, path, T).expect(path);
        assert!(
            body.starts_with(b"ok:"),
            "{path} must reach php, got {}",
            String::from_utf8_lossy(&body)
        );
    }

    let raw = http_get_raw(srv.addr, "/big.bin", &[("Range", "bytes=0-3")], T).expect("range GET");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 206"), "{text}");
    assert!(
        text.to_lowercase().contains("content-range: bytes 0-3/10"),
        "{text}"
    );
    assert_eq!(head_field(&raw, "content-length"), "4");
    assert!(text.ends_with("0123"), "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

/// The client sees no sign of the cache. A second request gives the same validators. A
/// conditional request gives 304 from the entry. A new version of the file appears after the
/// freshness window ends.
#[test]
fn cached_static_files_revalidate_over_the_wire() {
    let root = static_root(&[("app.css", "body{color:red}")]);
    let srv = spawn_with_http_extra(
        "shared/echo-worker.php",
        1,
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );

    let first = http_get_raw(srv.addr, "/app.css", &[], T).expect("GET /app.css");
    let etag = head_field(&first, "etag");
    let modified = head_field(&first, "last-modified");
    assert!(!etag.is_empty(), "no etag\n{}", diagnostics(&srv));
    assert!(
        !modified.is_empty(),
        "no last-modified\n{}",
        diagnostics(&srv)
    );

    let second = http_get_raw(srv.addr, "/app.css", &[], T).expect("second GET");
    assert_eq!(head_field(&second, "etag"), etag);
    assert_eq!(head_field(&second, "last-modified"), modified);
    assert_eq!(head_field(&second, "content-type"), "text/css");
    assert_eq!(head_field(&second, "content-length"), "15");
    let text = String::from_utf8_lossy(&second);
    assert!(text.ends_with("body{color:red}"), "{text}");

    for (name, value) in [
        ("If-None-Match", etag.as_str()),
        ("If-Modified-Since", modified.as_str()),
    ] {
        let raw = http_get_raw(srv.addr, "/app.css", &[(name, value)], T).expect("conditional GET");
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("HTTP/1.1 304"), "{name}: {text}");
        assert!(
            text.ends_with("\r\n\r\n"),
            "a 304 has no body: {name}: {text}"
        );
    }

    let head = http_raw_bytes(
        srv.addr,
        b"HEAD /app.css HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        T,
    )
    .expect("HEAD /app.css");
    let text = String::from_utf8_lossy(&head);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert_eq!(head_field(&head, "content-length"), "15");
    assert!(
        text.ends_with("\r\n\r\n"),
        "a HEAD response has no body: {text}"
    );

    // The new file has a different length and a different mtime. Either one causes the
    // reload.
    let path = root.join("app.css");
    std::fs::write(&path, "body{color:lime}").expect("rewrite");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open for mtime")
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000))
        .expect("set mtime");
    std::thread::sleep(Duration::from_millis(1100));

    let raw = http_get_raw(srv.addr, "/app.css", &[], T).expect("GET after the rewrite");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert_eq!(head_field(&raw, "content-length"), "16");
    let reloaded = head_field(&raw, "etag");
    assert!(!reloaded.is_empty(), "no etag after the reload: {text}");
    assert_ne!(reloaded, etag);
    assert!(text.ends_with("body{color:lime}"), "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

/// One connection serves sequential requests through the middleware chain:
/// miss to PHP, static hit, the same file from the cache, miss to PHP again.
#[test]
fn the_chain_serves_sequential_requests_on_one_connection() {
    let root = static_root(&[("app.css", "body{}")]);
    let srv = spawn_with_http_extra(
        "shared/echo-worker.php",
        1,
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );
    let mut c = Conn::open(srv.addr, T).expect("connect");

    fn content_length(fields: &[(String, String)]) -> usize {
        fields
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.parse().ok())
            .expect("content-length header")
    }

    c.send(b"GET /nope HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("first request");
    let (status, fields) = c.read_head(T).expect("first head");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    let body = c.read_n(content_length(&fields), T).expect("first body");
    assert!(body.starts_with(b"ok:"), "first body must come from php");

    c.send(b"GET /app.css HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("second request");
    let (status, fields) = c.read_head(T).expect("second head");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    let body = c.read_n(content_length(&fields), T).expect("second body");
    assert_eq!(body.as_slice(), b"body{}", "the hit must serve the file");

    // Only the cache can answer this request. A wrong content-length breaks the connection
    // here.
    std::fs::remove_file(root.join("app.css")).expect("remove the served file");
    c.send(b"GET /app.css HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("cached request");
    let (status, fields) = c.read_head(T).expect("cached head");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    let body = c.read_n(content_length(&fields), T).expect("cached body");
    assert_eq!(body.as_slice(), b"body{}", "the cache must serve the file");

    c.send(b"GET /nope HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("third request on the same connection");
    let (status, fields) = c.read_head(T).expect("reused connection must serve");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    let body = c.read_n(content_length(&fields), T).expect("third body");
    assert!(body.starts_with(b"ok:"), "third body must come from php");

    let _ = std::fs::remove_dir_all(&root);
}

/// One boot covers the routing: which paths and methods the middleware serves, which reach
/// PHP, and which fail without reaching PHP.
#[test]
fn static_files_route_each_request_over_the_wire() {
    let root = static_root(&[
        ("styles.css", "body{}"),
        ("data.bin", "abcdefghij"),
        ("index.html", "<h1>hi</h1>"),
        ("index.php", "<?php secret();"),
        ("Upper.PHP", "<?php upper();"),
        ("sub/index.php", "<?php sub();"),
        ("sub/index.html", "<h1>s</h1>"),
        ("assets/a.css", "a{}"),
        (".env", "SECRET=1"),
        (".git/config", "[core]"),
    ]);
    // A symlink loop fails with ELOOP, which is not a miss.
    std::os::unix::fs::symlink("loop.css", root.join("loop.css")).expect("symlink loop");
    let srv = spawn_echo_loop(&root, None);

    struct ToPhp {
        name: &'static str,
        path: String,
    }
    let long = format!("/{}", "a".repeat(300));
    let to_php = [
        ToPhp {
            name: "forbidden extension",
            path: "/index.php".into(),
        },
        ToPhp {
            name: "forbidden extension with an encoded dot",
            path: "/index%2Ephp".into(),
        },
        ToPhp {
            name: "forbidden extension in uppercase",
            path: "/Upper.PHP".into(),
        },
        ToPhp {
            name: "forbidden extension with an encoded trailing slash",
            path: "/index.php%2F".into(),
        },
        ToPhp {
            name: "forbidden extension with two encoded trailing slashes",
            path: "/index.php%2F%2F".into(),
        },
        ToPhp {
            name: "forbidden extension in a directory with an encoded trailing slash",
            path: "/sub/index.php%2F".into(),
        },
        ToPhp {
            name: "root directory",
            path: "/".into(),
        },
        ToPhp {
            name: "directory without a trailing slash",
            path: "/assets".into(),
        },
        ToPhp {
            name: "directory with a trailing slash",
            path: "/assets/".into(),
        },
        ToPhp {
            name: "directory with index.html without a trailing slash",
            path: "/sub".into(),
        },
        ToPhp {
            name: "directory with index.html with a trailing slash",
            path: "/sub/".into(),
        },
        ToPhp {
            name: "dotfile",
            path: "/.env".into(),
        },
        ToPhp {
            name: "file in a dot directory",
            path: "/.git/config".into(),
        },
        ToPhp {
            name: "dotfile with an encoded dot",
            path: "/%2Eenv".into(),
        },
        ToPhp {
            name: "parent directory",
            path: "/../outside.txt".into(),
        },
        ToPhp {
            name: "encoded parent directory",
            path: "/%2e%2e/outside.txt".into(),
        },
        ToPhp {
            name: "segment over NAME_MAX",
            path: long,
        },
        ToPhp {
            name: "NUL byte",
            path: "/a%00.css".into(),
        },
    ];
    for case in &to_php {
        let raw = http_get_raw(srv.addr, &case.path, &[], T).expect(case.name);
        let (status, body) = split_response(&raw);
        assert_eq!(status, 200, "{}\n{}", case.name, diagnostics(&srv));
        assert_eq!(
            head_field(&raw, "x-rapira-target"),
            case.path,
            "{} must reach php",
            case.name
        );
        assert_eq!(body, b"method=GET body=", "{}", case.name);
    }

    struct Served {
        name: &'static str,
        path: &'static str,
        body: &'static [u8],
    }
    let served = [
        Served {
            name: "index.html by its own path",
            path: "/index.html",
            body: b"<h1>hi</h1>",
        },
        Served {
            name: "file in a directory",
            path: "/assets/a.css",
            body: b"a{}",
        },
        Served {
            name: "query string",
            path: "/styles.css?v=2",
            body: b"body{}",
        },
    ];
    for case in &served {
        let raw = http_get_raw(srv.addr, case.path, &[], T).expect(case.name);
        let (status, body) = split_response(&raw);
        assert_eq!(status, 200, "{}\n{}", case.name, diagnostics(&srv));
        assert_eq!(head_field(&raw, "x-rapira-target"), "", "{}", case.name);
        assert_eq!(body, case.body, "{}", case.name);
    }

    // A HEAD with a NUL byte fails in the file-name arm. A GET answers 404 inside ServeDir.
    let raw = http_raw_bytes(
        srv.addr,
        b"HEAD /a%00.css HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n",
        T,
    )
    .expect("HEAD /a%00.css");
    assert_eq!(
        split_response(&raw).0,
        200,
        "{}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(head_field(&raw, "x-rapira-target"), "/a%00.css");
    assert_eq!(head_field(&raw, "content-type"), "text/plain");

    // The probe carries only the head. A lost request extension answers 500 in the http plugin.
    let raw = http_raw_bytes(
        srv.addr,
        b"GET /nope.txt HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\nContent-Length: 4\r\n\r\nping",
        T,
    )
    .expect("GET /nope.txt with a body");
    assert_eq!(split_response(&raw), (200, &b"method=GET body=ping"[..]));

    let (code, body) =
        http_post(srv.addr, "/styles.css", b"text/plain", b"p", T).expect("POST /styles.css");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(body, b"method=POST body=p");

    // RFC 9110 section 15.5.17, https://www.rfc-editor.org/rfc/rfc9110#section-15.5.17.
    // The field carries unsatisfied-range `"*/" complete-length` (section 14.4,
    // https://www.rfc-editor.org/rfc/rfc9110#section-14.4).
    let raw = http_get_raw(srv.addr, "/data.bin", &[("Range", "bytes=100-200")], T)
        .expect("unsatisfiable range");
    assert_eq!(
        split_response(&raw).0,
        416,
        "{}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(head_field(&raw, "content-range"), "bytes */10");
    assert_eq!(head_field(&raw, "x-rapira-target"), "");

    let raw = http_get_raw(srv.addr, "/loop.css", &[], T).expect("GET /loop.css");
    assert_eq!(
        split_response(&raw),
        (500, &b""[..]),
        "{}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(head_field(&raw, "x-rapira-target"), "");

    let _ = std::fs::remove_dir_all(&root);
}

/// `http.static.forbid` decides which sources stay with PHP. Each case boots its own master.
#[test]
fn the_forbid_list_decides_which_sources_reach_php() {
    struct Case {
        name: &'static str,
        forbid: &'static str,
        target: &'static str,
        body: &'static [u8],
    }
    let cases = [
        Case {
            name: "an uppercase entry blocks a lowercase file",
            forbid: "[\".PHP\"]",
            target: "/index.php",
            body: b"method=GET body=",
        },
        Case {
            name: "an empty list serves the source",
            forbid: "[]",
            target: "",
            body: b"<?php secret();",
        },
    ];
    for case in &cases {
        let root = static_root(&[("index.php", "<?php secret();")]);
        let srv = spawn_echo_loop(&root, Some(case.forbid));
        let raw = http_get_raw(srv.addr, "/index.php", &[], T).expect(case.name);
        assert_eq!(
            split_response(&raw),
            (200, case.body),
            "{}\n{}",
            case.name,
            diagnostics(&srv)
        );
        assert_eq!(
            head_field(&raw, "x-rapira-target"),
            case.target,
            "{}",
            case.name
        );
        drop(srv);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Each check deletes or rewrites the file after the first request, so only the cache can
/// give the old answer. The cache belongs to the one worker process. The entries go stale
/// after one second, so one wait covers every check that needs a stale entry.
#[test]
fn cached_entries_answer_until_they_go_stale() {
    let big = "x".repeat(262_145);
    let root = static_root(&[
        ("memory.css", "body{}"),
        ("data.bin", "abcdefghij"),
        ("empty.css", ""),
        ("big.bin", &big),
        ("race.bin", "abcdefghij"),
        ("kept.bin", "abcdefghij"),
        ("gone.css", "body{}"),
    ]);
    let srv = spawn_echo_loop(&root, None);
    let get = |path: &str, fields: &[(&str, &str)]| {
        http_get_raw(srv.addr, path, fields, T).unwrap_or_else(|e| panic!("GET {path}: {e}"))
    };
    let head = |path: &str| {
        let req = format!("HEAD {path} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n");
        http_raw_bytes(srv.addr, req.as_bytes(), T).unwrap_or_else(|e| panic!("HEAD {path}: {e}"))
    };

    let first = get("/memory.css", &[]);
    let etag = head_field(&first, "etag");
    let modified = head_field(&first, "last-modified");
    assert!(!etag.is_empty(), "no etag\n{}", diagnostics(&srv));
    assert!(!modified.is_empty(), "no last-modified");
    std::fs::remove_file(root.join("memory.css")).expect("remove memory.css");
    let raw = get("/memory.css", &[]);
    assert_eq!(split_response(&raw), (200, &b"body{}"[..]));
    assert_eq!(head_field(&raw, "x-rapira-target"), "");
    assert_eq!(head_field(&raw, "etag"), etag);
    assert_eq!(head_field(&raw, "last-modified"), modified);
    assert_eq!(head_field(&raw, "content-type"), "text/css");
    assert_eq!(head_field(&raw, "content-length"), "6");

    let first = get("/data.bin", &[]);
    let etag = head_field(&first, "etag");
    assert!(!etag.is_empty(), "no etag");
    std::fs::remove_file(root.join("data.bin")).expect("remove data.bin");
    // The range starts past the first byte, so the answer needs a seek in the cached body.
    // Content-Range carries first-pos "-" last-pos "/" complete-length (RFC 9110 section
    // 14.4, https://www.rfc-editor.org/rfc/rfc9110#section-14.4).
    let raw = get("/data.bin", &[("Range", "bytes=6-9")]);
    assert_eq!(split_response(&raw), (206, &b"ghij"[..]));
    assert_eq!(head_field(&raw, "content-range"), "bytes 6-9/10");
    let raw = get("/data.bin", &[("If-None-Match", &etag)]);
    assert_eq!(split_response(&raw), (304, &b""[..]));
    let raw = head("/data.bin");
    assert_eq!(split_response(&raw), (200, &b""[..]));
    assert_eq!(head_field(&raw, "content-length"), "10");
    assert_eq!(head_field(&raw, "x-rapira-target"), "");

    // The echo loop also answers 200, so only the content-length shows the cache hit.
    assert_eq!(split_response(&get("/empty.css", &[])).0, 200);
    std::fs::remove_file(root.join("empty.css")).expect("remove empty.css");
    let raw = get("/empty.css", &[]);
    assert_eq!(split_response(&raw), (200, &b""[..]));
    assert_eq!(head_field(&raw, "content-length"), "0");
    assert_eq!(head_field(&raw, "x-rapira-target"), "");

    // A file over the 256 KiB cap gets no entry.
    let raw = get("/big.bin", &[]);
    assert_eq!(head_field(&raw, "content-length"), "262145");
    assert_eq!(split_response(&raw), (200, big.as_bytes()));
    std::fs::remove_file(root.join("big.bin")).expect("remove big.bin");
    assert_eq!(head_field(&head("/big.bin"), "x-rapira-target"), "/big.bin");
    assert_eq!(
        head_field(&get("/big.bin", &[]), "x-rapira-target"),
        "/big.bin"
    );

    // Eight requests arrive for one cold path at the same time. Every answer matches.
    let gate = std::sync::Barrier::new(8);
    let answers: Vec<(String, Vec<u8>)> = std::thread::scope(|s| {
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    gate.wait();
                    let raw = get("/race.bin", &[]);
                    (head_field(&raw, "etag"), split_response(&raw).1.to_vec())
                })
            })
            .collect();
        tasks
            .into_iter()
            .map(|t| t.join().expect("race thread"))
            .collect()
    });
    for (etag, body) in &answers {
        assert!(!etag.is_empty(), "no etag");
        assert_eq!(etag, &answers[0].0);
        assert_eq!(body, b"abcdefghij");
    }

    // Both bodies have six bytes. Only the mtime shows the difference.
    let rewrite = root.join("rewrite.css");
    write_at(&rewrite, "aaaaaa", 1_000_000);
    let first = get("/rewrite.css", &[]);
    let rewrite_etag = head_field(&first, "etag");
    assert!(!rewrite_etag.is_empty(), "no etag");
    assert_eq!(split_response(&first).1, b"aaaaaa");
    write_at(&rewrite, "bbbbbb", 1_000_002);
    let inside = get("/rewrite.css", &[]);
    assert_eq!(head_field(&inside, "etag"), rewrite_etag);
    assert_eq!(split_response(&inside).1, b"aaaaaa");

    // Both writes keep the mtime. Only the length shows the difference.
    let resize = root.join("resize.css");
    write_at(&resize, "aaaaaa", 1_000_000);
    assert_eq!(split_response(&get("/resize.css", &[])).1, b"aaaaaa");
    write_at(&resize, "aaaaaaaaa", 1_000_000);

    assert_eq!(split_response(&get("/kept.bin", &[])).1, b"abcdefghij");

    assert_eq!(split_response(&get("/gone.css", &[])).1, b"body{}");
    std::fs::remove_file(root.join("gone.css")).expect("remove gone.css");
    let raw = get("/gone.css", &[]);
    assert_eq!(split_response(&raw), (200, &b"body{}"[..]));
    assert_eq!(head_field(&raw, "x-rapira-target"), "");

    // An entry is fresh for one second (TTL in crates/middleware/static_files/src/cache.rs).
    std::thread::sleep(Duration::from_millis(1100));

    let after = get("/rewrite.css", &[]);
    assert_ne!(head_field(&after, "etag"), rewrite_etag);
    assert_eq!(split_response(&after).1, b"bbbbbb");

    let after = get("/resize.css", &[]);
    assert_eq!(head_field(&after, "content-length"), "9");
    assert_eq!(split_response(&after).1, b"aaaaaaaaa");

    // A HEAD revalidation reads only the metadata. The body must survive it, so the GET after
    // the delete still answers from memory.
    assert_eq!(split_response(&head("/kept.bin")).0, 200);
    std::fs::remove_file(root.join("kept.bin")).expect("remove kept.bin");
    let raw = get("/kept.bin", &[]);
    assert_eq!(split_response(&raw), (200, &b"abcdefghij"[..]));
    assert_eq!(head_field(&raw, "x-rapira-target"), "");

    let raw = get("/gone.css", &[]);
    assert_eq!(head_field(&raw, "x-rapira-target"), "/gone.css");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_static_root_refuses_to_boot() {
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        "middleware = [\"static\"]\n[http.static]\nroot = \"/nonexistent-rapira-static-root\"\n",
    );
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(
        log.contains("http.static.root") && log.contains("is not accessible"),
        "{log}"
    );
}

/// A 000 directory passes a stat of its own path; the boot probe must resolve inside it.
#[test]
fn an_unreadable_static_root_refuses_to_boot() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir();
    let root = dir.join("root");
    std::fs::create_dir(&root).expect("create root");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).expect("chmod root");
    // Root bypasses permission checks; the case cannot occur for that user.
    if std::fs::read_dir(&root).is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("restore root");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(
        log.contains("http.static.root") && log.contains("is not accessible"),
        "{log}"
    );
}

/// A 0400 root lists but cannot resolve child paths; boot must require search permission.
#[test]
fn a_static_root_without_search_permission_refuses_to_boot() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir();
    let root = dir.join("root");
    std::fs::create_dir(&root).expect("create root");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o400)).expect("chmod root");
    // Root bypasses permission checks; the case cannot occur for that user.
    if std::fs::metadata(root.join(".")).is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("restore root");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(
        log.contains("http.static.root") && log.contains("is not accessible"),
        "{log}"
    );
}

#[test]
fn a_static_root_that_is_not_a_directory_refuses_to_boot() {
    let dir = scratch_dir();
    let file = dir.join("root");
    std::fs::write(&file, "x").expect("write file root");
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            file.display()
        ),
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(log.contains("is not a directory"), "{log}");
}

/// A dispatcher pool over the echo loop behind the static middleware on `root`. The echo
/// loop sets `x-rapira-target` and echoes the method and the body, so an answer from PHP is
/// visible. `forbid` is the TOML value of `http.static.forbid`. None leaves the key out.
fn spawn_echo_loop(root: &Path, forbid: Option<&str>) -> Server {
    let mut keys = format!(
        "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"",
        root.display()
    );
    if let Some(forbid) = forbid {
        keys += &format!("\nforbid = {forbid}");
    }
    Spawn::http(Mode::Dispatcher, fixture("dispatcher/echo-loop-worker.php"))
        .http_extra(&keys)
        .spawn()
}

/// The status code and the body of a close-delimited response.
fn split_response(raw: &[u8]) -> (u16, &[u8]) {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response head: {}", String::from_utf8_lossy(raw)));
    let status = String::from_utf8_lossy(&raw[..end])
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status: {}", String::from_utf8_lossy(raw)));
    (status, &raw[end + 4..])
}

/// Use whole seconds. An ext4 volume with 128-byte inodes has no nanosecond field, so it
/// keeps only the seconds of the mtime.
/// https://www.kernel.org/doc/html/latest/filesystems/ext4/inodes.html#inode-timestamps
fn write_at(path: &Path, contents: &str, mtime_secs: u64) {
    std::fs::write(path, contents).expect("write static file");
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for mtime")
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs))
        .expect("set mtime");
}
