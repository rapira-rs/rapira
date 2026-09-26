//! The descriptor set behind a gRPC pool: what `Schema::load` accepts, and the JSON transcoding that Connect clients get from it.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http::Method;
use rapira_grpc::{MethodInfo, Schema, ServiceInfo};
use tests::grpc::{
    Conn, ECHO_PATH, ECHO_SERVICE, HI, Wire, config, respond, scratch_dir, start, tcp,
};

fn load(path: &Path, services: Option<&[&str]>) -> anyhow::Result<Schema> {
    let services: Option<Vec<String>> =
        services.map(|names| names.iter().map(|&s| s.to_owned()).collect());
    Schema::load(path, services.as_deref())
}

/// The listing keeps streaming methods, in echo.proto order. echo.proto imports dep.proto, so the default list skips DepService.
#[test]
fn services_report_every_configured_method() {
    let method = |name: &str, server_streaming| MethodInfo {
        name: name.to_owned(),
        input_type: "rapira.test.v1.EchoRequest".to_owned(),
        output_type: "rapira.test.v1.EchoResponse".to_owned(),
        client_streaming: false,
        server_streaming,
    };
    let want = [ServiceInfo {
        name: ECHO_SERVICE.to_owned(),
        methods: vec![
            method("Echo", false),
            method("Get", false),
            method("Watch", true),
        ],
    }];
    let listed =
        load(&tests::echo_descriptor_set(), Some(&[ECHO_SERVICE])).expect("echo.binpb loads");
    assert_eq!(listed.services(), want, "an explicit list narrows the set");

    let every = load(&tests::echo_descriptor_set(), None).expect("echo.binpb loads");
    let names: Vec<&str> = every.services().iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        ["rapira.test.v1.EchoService", "rapira.test.v1.OtherService"],
        "no list serves every service of the files that no other file imports, in descriptor order"
    );
    assert_eq!(every.services()[0], want[0]);

    let dep = load(
        &tests::echo_descriptor_set(),
        Some(&["rapira.test.dep.v1.DepService"]),
    )
    .expect("echo.binpb loads");
    assert_eq!(
        dep.services()[0].name,
        "rapira.test.dep.v1.DepService",
        "an explicit list can name the service of an imported file"
    );
}

#[test]
fn load_rejects_a_set_it_cannot_serve() {
    struct Case {
        name: &'static str,
        path: PathBuf,
        service: &'static str,
        error: &'static str,
    }
    let dir = scratch_dir("schema");
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
            path: tests::fixture("grpc/echo-no-imports.binpb"),
            service: ECHO_SERVICE,
            error: "--include_imports",
        },
        Case {
            name: "set without an import that supplies only an option",
            path: tests::fixture("grpc_options/ping-no-imports.binpb"),
            service: "rapira.test.options.v1.PingService",
            error: "--include_imports",
        },
    ];
    for case in cases {
        let Err(err) = load(&case.path, Some(&[case.service])) else {
            panic!("{}: the set loaded", case.name);
        };
        let err = err.to_string();
        assert!(err.contains(case.error), "{}: {err}", case.name);
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
        let Err(err) = load(&tests::echo_descriptor_set(), Some(&case.services)) else {
            panic!("{}: the set loaded", case.name);
        };
        let err = err.to_string();
        assert!(
            err.contains("grpc.services lists `rapira.test.v1.EchoService` twice"),
            "{}: {err}",
            case.name
        );
    }
}

/// A Connect JSON call transcodes by the descriptor: the request into the bytes PHP gets, the reply out of the bytes PHP returns.
///
/// Sources: the protobuf encoding (field 1 string "hi" is 0a 02 68 69, field 2 Timestamp{seconds: 1} is 12 02 08 01, field 3 packed repeated int32 is 1a, the varint length, then one varint per element), proto3 JSON (a repeated int32 is an array of numbers, defaults are omitted, unknown fields are ignored) and the Connect protocol (400 for a request that does not decode, 500 for a reply that does not).
#[tokio::test]
async fn json_transcodes_by_descriptor() {
    // buffa charges the size of its Value type, at least 64 bytes, per element against a 32 MiB budget for untrusted input, so at most 524,288 elements fit. The reply is the application's, so the budget does not apply to it.
    const IDS: usize = 600_000;
    let mut many_ids = vec![0x1a, 0xc0, 0xcf, 0x24];
    many_ids.resize(many_ids.len() + IDS, 0x01);
    let many_ids_json = format!(r#"{{"ids":[{}1]}}"#, "1,".repeat(IDS - 1));
    struct Case<'a> {
        name: &'static str,
        /// The bytes PHP replies with. None: PHP echoes the message.
        answer: Option<Bytes>,
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
            answer: Some(Bytes::from_static(&[0xff])),
            request: br#"{"text":"hi"}"#,
            status: 500,
            sent: Some(HI),
            reply: None,
        },
        Case {
            name: "reply above the untrusted-input element budget",
            answer: Some(Bytes::from(many_ids.clone())),
            request: br#"{"text":"hi"}"#,
            status: 200,
            sent: Some(HI),
            reply: Some(many_ids_json.as_bytes()),
        },
    ];

    let (mut running, mut calls) = start(config(tcp()));
    let mut conn = Conn::open(&running.listen, Wire::Http1)
        .await
        .expect("connect");
    for case in cases {
        let send = conn.send(
            Method::POST,
            ECHO_PATH,
            &[("content-type", "application/json")],
            case.request,
        );
        let got = match case.sent {
            Some(sent) => {
                let (got, ()) = tokio::join!(send, async {
                    let call = calls.recv().await.expect("the call reached PHP");
                    assert_eq!(
                        &call.message()[..],
                        sent,
                        "{}: the message PHP got",
                        case.name
                    );
                    let reply = case
                        .answer
                        .clone()
                        .unwrap_or_else(|| call.message().clone());
                    respond(call, Ok(reply));
                });
                got
            }
            None => send.await,
        };
        let got = got.unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

        assert_eq!(got.status, case.status, "{}: {got:?}", case.name);
        assert!(calls.try_recv().is_err(), "{}: a stray call", case.name);
        if let Some(reply) = case.reply {
            assert!(
                &got.body[..] == reply,
                "{}: {}",
                case.name,
                String::from_utf8_lossy(&got.body[..got.body.len().min(200)])
            );
        }
    }
    running.shutdown().await.unwrap();
}
