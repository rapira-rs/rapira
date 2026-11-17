//! The descriptor set behind a gRPC pool: what the boot accepts, and the JSON transcoding that Connect clients get from it.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use http::Method;
use serde_json::{Value, json};
use tests::grpc::{Conn, ECHO_PATH, ECHO_SERVICE, HI, Wire};
use tests::{fixture, server_log};

use crate::harness::{Exited, Spawn, calls, listen, scratch_dir, stop};

/// The services that `getServices()` lists in a pool over echo.binpb that serves `services`.
fn services_of(services: Option<&[&str]>) -> Vec<Value> {
    let srv = Spawn::grpc(fixture("grpc/identity.php"))
        .services(services)
        .json_log()
        .spawn();
    let ctx: Value =
        serde_json::from_str(&server_log::wait_app_record(&srv.log_file(), "dispatcher"))
            .expect("the dispatcher record holds JSON");
    ctx["services"].as_array().expect("a service list").clone()
}

/// The exit of a boot over `path` that serves `services`.
fn boot_failure(path: &Path, services: &[&str]) -> Exited {
    Spawn::grpc(fixture("grpc/identity.php"))
        .descriptor_set(path)
        .services(Some(services))
        .boot_failure()
}

/// The listing keeps streaming methods, in echo.proto order. echo.proto imports dep.proto, so the default list skips DepService.
///
/// Expected values: echo.proto, and MethodInfo and MethodKind of the contract.
#[test]
fn services_report_every_configured_method() {
    let method = |name: &str, kind: &str| {
        json!({
            "class": "Rapira\\Grpc\\MethodInfo",
            "name": name,
            "inputType": "rapira.test.v1.EchoRequest",
            "outputType": "rapira.test.v1.EchoResponse",
            "kind": kind,
        })
    };
    let want = json!({
        "class": "Rapira\\Grpc\\ServiceInfo",
        "name": ECHO_SERVICE,
        "methods": [
            method("Echo", "unary"),
            method("Get", "unary"),
            method("Watch", "server-streaming"),
        ],
    });
    let listed = services_of(Some(&[ECHO_SERVICE]));
    assert_eq!(
        listed,
        std::slice::from_ref(&want),
        "an explicit list narrows the set"
    );

    let every = services_of(None);
    let names: Vec<&str> = every.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(
        names,
        ["rapira.test.v1.EchoService", "rapira.test.v1.OtherService"],
        "no list serves every service of the files that no other file imports, in descriptor order"
    );
    assert_eq!(every[0], want);

    let dep = services_of(Some(&["rapira.test.dep.v1.DepService"]));
    assert_eq!(
        dep[0]["name"], "rapira.test.dep.v1.DepService",
        "an explicit list can name the service of an imported file"
    );
}

/// The master loads the schema before the fork, and `main` returns the error, so the process exits 1.
#[test]
fn load_rejects_a_set_it_cannot_serve() {
    struct Case {
        name: &'static str,
        path: PathBuf,
        service: &'static str,
        error: &'static str,
    }
    let dir = scratch_dir();
    let not_a_set = dir.join("not-a-set.binpb");
    std::fs::write(&not_a_set, [0xff, 0xff]).unwrap();
    let cases = [
        Case {
            name: "unknown service",
            path: tests::echo_descriptor_set(),
            service: "rapira.test.v1.Missing",
            error: "grpc.services entry `rapira.test.v1.Missing` is not in",
        },
        Case {
            name: "plugin health service",
            path: tests::echo_descriptor_set(),
            service: "grpc.health.v1.Health",
            error: "grpc.services entry `grpc.health.v1.Health` is served by the plugin",
        },
        Case {
            name: "plugin reflection service",
            path: tests::echo_descriptor_set(),
            service: "grpc.reflection.v1.ServerReflection",
            error: "grpc.services entry `grpc.reflection.v1.ServerReflection` is served by the plugin",
        },
        Case {
            name: "missing file",
            path: PathBuf::from("/nonexistent.binpb"),
            service: ECHO_SERVICE,
            error: "reading grpc.descriptor_set",
        },
        Case {
            name: "not a set",
            path: not_a_set,
            service: ECHO_SERVICE,
            error: "--include_imports",
        },
        Case {
            name: "set without its imports",
            path: fixture("grpc/echo-no-imports.binpb"),
            service: ECHO_SERVICE,
            error: "--include_imports",
        },
        Case {
            name: "set without an import that supplies only an option",
            path: fixture("grpc_options/ping-no-imports.binpb"),
            service: "rapira.test.options.v1.PingService",
            error: "--include_imports",
        },
    ];
    for case in cases {
        let Exited { status, log } = boot_failure(&case.path, &[case.service]);
        assert_eq!(status.code(), Some(1), "{}: {log}", case.name);
        assert!(log.contains(case.error), "{}: {log}", case.name);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// buffa resolves a name with a leading dot to the same service.
#[test]
fn load_rejects_a_service_listed_twice() {
    struct Case {
        name: &'static str,
        services: [&'static str; 2],
    }
    let cases = [
        Case {
            name: "exact duplicate",
            services: [ECHO_SERVICE, ECHO_SERVICE],
        },
        Case {
            name: "leading-dot alias",
            services: [ECHO_SERVICE, ".rapira.test.v1.EchoService"],
        },
    ];
    for case in cases {
        let Exited { status, log } = boot_failure(&tests::echo_descriptor_set(), &case.services);
        assert_eq!(status.code(), Some(1), "{}: {log}", case.name);
        assert!(
            log.contains("grpc.services lists `rapira.test.v1.EchoService` twice"),
            "{}: {log}",
            case.name
        );
    }
}

/// A loopback HTTP server that answers one request.
struct Remote {
    port: u16,
    /// Returns the request line when the answer is sent.
    request_line: JoinHandle<String>,
}

/// Answers the first request with `status` and `body`.
fn remote(status: &'static str, body: Vec<u8>) -> Remote {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the image server");
    let port = listener
        .local_addr()
        .expect("the image server address")
        .port();
    let request_line = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        let mut head = BufReader::new(conn.try_clone().expect("clone the connection"));
        let mut request_line = String::new();
        head.read_line(&mut request_line).expect("request line");
        // The head ends at the empty line.
        let mut line = String::new();
        while head.read_line(&mut line).expect("header line") > 2 {
            line.clear();
        }
        write!(
            conn,
            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .expect("write the head");
        conn.write_all(&body).expect("write the body");
        request_line
    });
    Remote { port, request_line }
}

/// The master downloads a `descriptor_set` URL at boot, and the pool routes by the downloaded image.
///
/// Expected values: a Buf image input that starts with `http://` is a remote location (https://buf.build/docs/reference/inputs/#image-formats), and the echo of `json_transcodes_by_descriptor`.
#[tokio::test]
async fn a_set_loads_from_a_url() {
    let image = std::fs::read(tests::echo_descriptor_set()).expect("read echo.binpb");
    let remote = remote("200 OK", image);
    let url = format!("http://127.0.0.1:{}/sets/echo.binpb", remote.port);
    let srv = Spawn::grpc(fixture("grpc/wire-worker.php"))
        .descriptor_set(Path::new(&url))
        .spawn();
    assert_eq!(
        remote.request_line.join().expect("the image server"),
        "GET /sets/echo.binpb HTTP/1.1\r\n"
    );

    let mut conn = Conn::open(&listen(&srv), Wire::Http1)
        .await
        .expect("connect");
    let got = conn
        .send(
            Method::POST,
            ECHO_PATH,
            &[("content-type", "application/json")],
            br#"{"text":"hi"}"#,
        )
        .await
        .expect("the call");
    assert_eq!(got.status, 200, "{got:?}");
    assert_eq!(&got.body[..], br#"{"text":"hi"}"#);
    drop(conn);
    stop(srv).await;
}

/// A URL that the master cannot load fails the boot like a local set does. The log shows the URL without its userinfo and its query, because both can hold credentials.
///
/// Expected values: exit 1 as in `load_rejects_a_set_it_cannot_serve`, and the status text of ureq (`http status: 404`).
#[test]
fn load_rejects_a_url_it_cannot_load() {
    struct Case {
        name: &'static str,
        status: &'static str,
        body: &'static [u8],
        /// The step that fails, as the log names it.
        step: &'static str,
        cause: &'static str,
    }
    let cases = [
        Case {
            name: "status 404",
            status: "404 Not Found",
            body: b"",
            step: "downloading",
            cause: "http status: 404",
        },
        Case {
            name: "not a set",
            status: "200 OK",
            body: &[0xff, 0xff],
            step: "decoding",
            cause: "--include_imports",
        },
    ];
    for case in cases {
        let remote = remote(case.status, case.body.to_vec());
        let url = format!(
            "http://user:secret@127.0.0.1:{}/api.binpb?sig=secret",
            remote.port
        );
        let Exited { status, log } = boot_failure(Path::new(&url), &[ECHO_SERVICE]);
        assert_eq!(status.code(), Some(1), "{}: {log}", case.name);
        let failed = format!(
            "{} grpc.descriptor_set http://127.0.0.1:{}/api.binpb: ",
            case.step, remote.port
        );
        assert!(log.contains(&failed), "{}: {log}", case.name);
        assert!(log.contains(case.cause), "{}: {log}", case.name);
        assert!(!log.contains("secret"), "{}: {log}", case.name);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A Connect JSON call transcodes by the descriptor: the request into the bytes PHP gets, the reply out of the bytes PHP returns.
///
/// Sources: the protobuf encoding (field 1 string "hi" is 0a 02 68 69, field 2 Timestamp{seconds: 1} is 12 02 08 01, field 3 packed repeated int32 is 1a, the varint length, then one varint per element), proto3 JSON (a repeated int32 is an array of numbers, defaults are omitted, unknown fields are ignored) and the Connect protocol (400 for a request that does not decode, 500 for a reply that does not).
#[tokio::test]
async fn json_transcodes_by_descriptor() {
    // buffa charges the size of its Value type, at least 64 bytes, per element against a 32 MiB budget for untrusted input, so at most 524,288 elements fit. The reply is the application's, so the budget does not apply to it.
    // grpc/wire-worker.php replies with this many elements for `x-reply: many-ids`.
    const IDS: usize = 600_000;
    let many_ids_json = format!(r#"{{"ids":[{}1]}}"#, "1,".repeat(IDS - 1));
    struct Case<'a> {
        name: &'static str,
        /// The `x-reply` value that selects the bytes PHP replies with. None: PHP echoes the message.
        answer: Option<&'static str>,
        request: &'a [u8],
        status: u16,
        /// The bytes PHP gets. None: PHP never sees the call.
        sent: Option<&'a [u8]>,
        /// None: not checked.
        reply: Option<&'a [u8]>,
    }
    let cases = [
        Case {
            name: "json to proto and back",
            answer: None,
            request: br#"{"text":"hi"}"#,
            status: 200,
            sent: Some(HI),
            reply: Some(br#"{"text":"hi"}"#),
        },
        Case {
            name: "well-known type both ways",
            answer: None,
            request: br#"{"text":"hi","at":"1970-01-01T00:00:01Z"}"#,
            status: 200,
            sent: Some(&[0x0a, 0x02, 0x68, 0x69, 0x12, 0x02, 0x08, 0x01]),
            reply: Some(br#"{"text":"hi","at":"1970-01-01T00:00:01Z"}"#),
        },
        Case {
            name: "unknown field ignored",
            answer: None,
            request: br#"{"text":"hi","x":1}"#,
            status: 200,
            sent: Some(HI),
            reply: Some(br#"{"text":"hi"}"#),
        },
        Case {
            name: "defaults omitted",
            answer: None,
            request: b"{}",
            status: 200,
            sent: Some(b""),
            reply: Some(b"{}"),
        },
        Case {
            name: "wrong json type",
            answer: None,
            request: br#"{"text":1}"#,
            status: 400,
            sent: None,
            reply: None,
        },
        Case {
            name: "not json",
            answer: None,
            request: b"not json",
            status: 400,
            sent: None,
            reply: None,
        },
        Case {
            name: "invalid utf-8",
            answer: None,
            request: b"{\"text\":\"\xff\"}",
            status: 400,
            sent: None,
            reply: None,
        },
        Case {
            name: "undecodable reply",
            answer: Some("ff"),
            request: br#"{"text":"hi"}"#,
            status: 500,
            sent: Some(HI),
            reply: None,
        },
        Case {
            name: "reply above the untrusted-input element budget",
            answer: Some("many-ids"),
            request: br#"{"text":"hi"}"#,
            status: 200,
            sent: Some(HI),
            reply: Some(many_ids_json.as_bytes()),
        },
    ];

    let srv = Spawn::grpc(fixture("grpc/wire-worker.php"))
        .json_log()
        .spawn();
    let mut conn = Conn::open(&listen(&srv), Wire::Http1)
        .await
        .expect("connect");
    for case in cases {
        let before = calls(&srv).len();
        let mut headers = vec![("content-type", "application/json")];
        headers.extend(case.answer.map(|answer| ("x-reply", answer)));
        let got = conn
            .send(Method::POST, ECHO_PATH, &headers, case.request)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.status, case.status, "{}: {got:?}", case.name);
        let sent: Vec<Value> = calls(&srv)[before..]
            .iter()
            .map(|call| call["message"].clone())
            .collect();
        let want: Vec<Value> = case
            .sent
            .map(|bytes| json!(hex(bytes)))
            .into_iter()
            .collect();
        assert_eq!(sent, want, "{}: the message PHP got", case.name);
        if let Some(reply) = case.reply {
            assert!(
                &got.body[..] == reply,
                "{}: {}",
                case.name,
                String::from_utf8_lossy(&got.body[..got.body.len().min(200)])
            );
        }
    }
    drop(conn);
    stop(srv).await;
}
