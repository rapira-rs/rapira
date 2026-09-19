use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes};
use extension_api::{
    Addr, Backend, BoxError, BoxFuture, FieldLines, HttpRequest, HttpResponse, Middleware, Next,
    Peer, Php, Protocol, Rejection, grpc,
};
use http::HeaderMap;
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Full, StreamBody};
use prost::Message;
use tokio::sync::Notify;
use tonic_reflection::pb::v1 as pb;

use crate::{Config, Registry, Server, handler};

mod reflection;

fn registry() -> Arc<Registry> {
    static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let imports = dir.path().join("imports");
        std::fs::create_dir(&app).unwrap();
        std::fs::create_dir(&imports).unwrap();
        std::fs::write(
            app.join("echo.proto"),
            r#"
            syntax = "proto3";
            package example;
            import "vendor.proto";
            import "google/protobuf/descriptor.proto";
            extend google.protobuf.MethodOptions { string note = 50001; }
            service Echo {
                rpc Call(vendor.Input) returns (vendor.Output) { option (note) = "kept"; }
            }
        "#,
        )
        .unwrap();
        std::fs::write(
            imports.join("vendor.proto"),
            r#"
            syntax = "proto3";
            package vendor;
            message Input { bytes value = 1; }
            message Output { bytes value = 1; }
            service Hidden { rpc Watch(stream Input) returns (stream Output); }
        "#,
        )
        .unwrap();
        Arc::new(Registry::load(&[app], &[imports]).unwrap())
    }))
}

fn config() -> Config {
    Config {
        listen: extension_api::ListenAddr::Tcp(([127, 0, 0, 1], 0).into()),
        registry: registry(),
        reflection: true,
        interceptors: Vec::new(),
        gzip_responses: false,
        max_request_message_size: 4 * 1024 * 1024,
        max_response_message_size: 4 * 1024 * 1024,
        drain_grace: Duration::from_secs(1),
    }
}

#[derive(Default)]
struct TestBackend {
    replies: Mutex<VecDeque<anyhow::Result<grpc::Reply>>>,
    seen: Mutex<Vec<grpc::Request>>,
    hold: bool,
    started: Notify,
    release: Notify,
    dropped: Arc<AtomicBool>,
}

struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Backend for TestBackend {
    fn exec(
        &self,
        _: extension_api::Request,
    ) -> BoxFuture<'_, anyhow::Result<extension_api::Reply>> {
        unreachable!("gRPC uses exec_grpc")
    }

    fn exec_grpc(&self, req: grpc::Request) -> BoxFuture<'_, anyhow::Result<grpc::Reply>> {
        Box::pin(async move {
            let message = req.message.clone();
            self.seen.lock().unwrap().push(req);
            if self.hold {
                let _dropped = Dropped(Arc::clone(&self.dropped));
                self.started.notify_one();
                self.release.notified().await;
            }
            self.replies.lock().unwrap().pop_front().unwrap_or_else(|| {
                Ok(grpc::Reply {
                    headers: Vec::new(),
                    trailers: Vec::new(),
                    result: Ok(message),
                })
            })
        })
    }
}

fn shared(cfg: Config, backend: &Arc<TestBackend>) -> Arc<handler::Shared> {
    Arc::new(handler::Shared::new(cfg, Php::new(backend.clone())).unwrap())
}

fn request(path: &str, wire: impl Into<Bytes>) -> HttpRequest {
    http::Request::builder()
        .method("POST")
        .uri(path)
        .version(http::Version::HTTP_2)
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(
            Full::new(wire.into())
                .map_err(BoxError::from)
                .boxed_unsync(),
        )
        .unwrap()
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0];
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

async fn call(shared: Arc<handler::Shared>, req: HttpRequest) -> HttpResponse {
    handler::handle(
        shared,
        req,
        Addr::Inet(([127, 0, 0, 1], 41000).into()),
        Addr::Inet(([127, 0, 0, 1], 9001).into()),
    )
    .await
}

async fn collect(res: HttpResponse) -> (HeaderMap, Bytes, HeaderMap) {
    assert_eq!(res.status(), http::StatusCode::OK);
    let (parts, body) = res.into_parts();
    let collected = body.collect().await.unwrap();
    let trailers = collected.trailers().cloned().unwrap_or_default();
    (parts.headers, collected.to_bytes(), trailers)
}

fn status(headers: &HeaderMap, trailers: &HeaderMap) -> tonic::Status {
    tonic::Status::from_header_map(trailers)
        .or_else(|| tonic::Status::from_header_map(headers))
        .expect("a terminal gRPC status")
}

fn values(map: &HeaderMap, name: &str) -> Vec<Vec<u8>> {
    map.get_all(name)
        .iter()
        .map(|value| value.as_bytes().to_vec())
        .collect()
}

#[tokio::test]
async fn unary_payload_cardinality_and_framing() {
    struct Case {
        name: &'static str,
        wire: &'static [u8],
        code: tonic::Code,
        message: Option<&'static [u8]>,
    }
    let cases = [
        Case {
            name: "binary_payload",
            wire: b"\0\0\0\0\x04\x0a\x02\0\xff",
            code: tonic::Code::Ok,
            message: Some(b"\x0a\x02\0\xff"),
        },
        Case {
            name: "zero_byte_message",
            wire: b"\0\0\0\0\0",
            code: tonic::Code::Ok,
            message: Some(b""),
        },
        Case {
            name: "missing_message",
            wire: b"",
            code: tonic::Code::Internal,
            message: None,
        },
        Case {
            name: "second_empty_message",
            wire: b"\0\0\0\0\0\0\0\0\0\0",
            code: tonic::Code::Internal,
            message: None,
        },
        Case {
            name: "second_message_missing_payload",
            wire: b"\0\0\0\0\0\0\0\0\0\x01",
            code: tonic::Code::Internal,
            message: None,
        },
        Case {
            name: "partial_second_header",
            wire: b"\0\0\0\0\0\0\0",
            code: tonic::Code::Internal,
            message: None,
        },
        Case {
            name: "partial_first_header",
            wire: b"\0\0",
            code: tonic::Code::Internal,
            message: None,
        },
        Case {
            name: "partial_payload",
            wire: b"\0\0\0\0\x02x",
            code: tonic::Code::Internal,
            message: None,
        },
        Case {
            name: "invalid_compression_flag",
            wire: b"\x02\0\0\0\0",
            code: tonic::Code::Internal,
            message: None,
        },
        Case {
            name: "missing_compression_encoding",
            wire: b"\x01\0\0\0\0",
            code: tonic::Code::Internal,
            message: None,
        },
        Case {
            name: "request_limit",
            wire: b"\0\0\0\0\x09abcdefghi",
            code: tonic::Code::OutOfRange,
            message: None,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let cfg = Config {
            max_request_message_size: 8,
            ..config()
        };
        let (headers, body, trailers) = collect(
            call(
                shared(cfg, &backend),
                request("/example.Echo/Call", case.wire),
            )
            .await,
        )
        .await;
        assert_eq!(
            status(&headers, &trailers).code(),
            case.code,
            "{}",
            case.name
        );
        let seen = backend.seen.lock().unwrap();
        if let Some(message) = case.message {
            assert_eq!(seen.len(), 1, "{}", case.name);
            assert_eq!(seen[0].message.as_ref(), message, "{}", case.name);
            assert_eq!(seen[0].method, "example.Echo/Call", "{}", case.name);
            assert_eq!(seen[0].remote, Addr::Inet(([127, 0, 0, 1], 41000).into()));
            assert_eq!(body.as_ref(), case.wire, "{}", case.name);
            assert!(headers.get("grpc-status").is_none(), "{}", case.name);
        } else {
            assert!(seen.is_empty(), "{} reached PHP", case.name);
            assert!(body.is_empty(), "{}", case.name);
        }
    }
}

#[tokio::test]
async fn identity_body_preserves_payload_allocation_and_frame_state() {
    struct Case {
        name: &'static str,
        message: Bytes,
        prefix: &'static [u8; 5],
    }
    let cases = [
        Case {
            name: "empty_payload",
            message: Bytes::new(),
            prefix: b"\0\0\0\0\0",
        },
        Case {
            name: "binary_payload",
            message: Bytes::from(vec![0, 255, 128, 1]),
            prefix: b"\0\0\0\0\x04",
        },
        Case {
            name: "eight_byte_payload",
            message: Bytes::from(vec![0, 1, 2, 3, 4, 5, 6, 7]),
            prefix: b"\0\0\0\0\x08",
        },
        Case {
            name: "slice_of_owned_payload",
            message: Bytes::from(vec![9, 0, 255, 128, 1, 9]).slice(1..5),
            prefix: b"\0\0\0\0\x04",
        },
    ];
    for case in cases {
        let mut body = crate::codec::IdentityBody::new(case.message.clone()).unwrap();
        assert!(!body.is_end_stream(), "{}", case.name);
        let prefix = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(prefix.as_ref(), case.prefix, "{}", case.name);
        assert!(!body.is_end_stream(), "{}", case.name);
        if !case.message.is_empty() {
            let payload = body.frame().await.unwrap().unwrap().into_data().unwrap();
            assert_eq!(payload, case.message, "{}", case.name);
            assert_eq!(
                payload.as_ptr(),
                case.message.as_ptr(),
                "{}: payload allocation",
                case.name
            );
        }
        assert!(!body.is_end_stream(), "{}", case.name);
        let trailers = body
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap();
        assert_eq!(trailers.len(), 1, "{}", case.name);
        assert_eq!(trailers["grpc-status"], "0", "{}", case.name);
        assert!(body.is_end_stream(), "{}", case.name);
        assert!(body.frame().await.is_none(), "{}", case.name);
        assert!(body.frame().await.is_none(), "{}", case.name);
    }
}

#[tokio::test]
async fn identity_response_preserves_owned_payload_and_metadata() {
    struct Case {
        name: &'static str,
        message: &'static [u8],
        prefix: &'static [u8; 5],
        headers: &'static [&'static str],
        trailers: &'static [&'static str],
        cancel: bool,
    }
    let cases = [
        Case {
            name: "binary_payload",
            message: b"\0\xff\x80\x01",
            prefix: b"\0\0\0\0\x04",
            headers: &[],
            trailers: &[],
            cancel: false,
        },
        Case {
            name: "empty_payload",
            message: b"",
            prefix: b"\0\0\0\0\0",
            headers: &[],
            trailers: &[],
            cancel: false,
        },
        Case {
            name: "at_response_limit",
            message: b"\0\x01\x02\x03\x04\x05\x06\x07",
            prefix: b"\0\0\0\0\x08",
            headers: &[],
            trailers: &[],
            cancel: false,
        },
        Case {
            name: "initial_metadata",
            message: b"\0\xff\x80\x01",
            prefix: b"\0\0\0\0\x04",
            headers: &["head-one", "head-two"],
            trailers: &[],
            cancel: false,
        },
        Case {
            name: "repeated_trailers",
            message: b"\0\xff\x80\x01",
            prefix: b"\0\0\0\0\x04",
            headers: &[],
            trailers: &["tail-one", "tail-two"],
            cancel: false,
        },
        Case {
            name: "cancel_before_consumption",
            message: b"\0\xff\x80\x01",
            prefix: b"\0\0\0\0\x04",
            headers: &[],
            trailers: &[],
            cancel: true,
        },
    ];
    for case in cases {
        let message_owner: Arc<[u8]> = Arc::from(case.message);
        let payload_ptr = message_owner.as_ptr();
        let owner = Arc::downgrade(&message_owner);
        let message = Bytes::from_owner(message_owner);
        let backend = Arc::new(TestBackend::default());
        backend.replies.lock().unwrap().push_back(Ok(grpc::Reply {
            headers: case
                .headers
                .iter()
                .map(|value| ("x-stage".into(), value.as_bytes().to_vec()))
                .collect(),
            trailers: case
                .trailers
                .iter()
                .map(|value| ("x-stage".into(), value.as_bytes().to_vec()))
                .collect(),
            result: Ok(message),
        }));
        let shared = shared(
            Config {
                max_response_message_size: 8,
                ..config()
            },
            &backend,
        );
        let response = call(
            Arc::clone(&shared),
            request("/example.Echo/Call", b"\0\0\0\0\0".as_slice()),
        )
        .await;
        assert_eq!(response.status(), http::StatusCode::OK, "{}", case.name);
        let headers = response.headers();
        assert_eq!(headers["content-type"], "application/grpc", "{}", case.name);
        assert!(!headers.contains_key("grpc-encoding"), "{}", case.name);
        assert!(!headers.contains_key("grpc-status"), "{}", case.name);
        assert_eq!(
            headers
                .get_all("x-stage")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            case.headers,
            "{}",
            case.name
        );
        assert_eq!(shared.inflight.load(Ordering::Acquire), 1, "{}", case.name);
        assert_eq!(owner.strong_count(), 1, "{}", case.name);
        if case.cancel {
            drop(response);
        } else {
            let collected = response.into_body().collect().await.unwrap();
            let trailers = collected.trailers().expect("status trailers");
            assert_eq!(
                values(trailers, "grpc-status"),
                [b"0".to_vec()],
                "{}",
                case.name
            );
            assert_eq!(
                trailers
                    .get_all("x-stage")
                    .iter()
                    .map(|value| value.to_str().unwrap())
                    .collect::<Vec<_>>(),
                case.trailers,
                "{}",
                case.name
            );
            let mut wire = collected.aggregate();
            assert_eq!(wire.copy_to_bytes(5).as_ref(), case.prefix, "{}", case.name);
            assert_eq!(wire.remaining(), case.message.len(), "{}", case.name);
            if !case.message.is_empty() {
                assert_eq!(wire.chunk(), case.message, "{}", case.name);
                assert_eq!(
                    wire.chunk().as_ptr(),
                    payload_ptr,
                    "{}: payload allocation",
                    case.name
                );
                assert_eq!(owner.strong_count(), 1, "{}", case.name);
            }
        }
        assert_eq!(shared.inflight.load(Ordering::Acquire), 0, "{}", case.name);
        assert_eq!(owner.strong_count(), 0, "{}", case.name);
    }
}

#[tokio::test]
async fn identity_responses_keep_a_reused_http2_connection_open() {
    struct Case {
        name: &'static str,
        wire: &'static [u8],
    }
    let cases = [
        Case {
            name: "empty_payload",
            wire: b"\0\0\0\0\0",
        },
        Case {
            name: "binary_payload",
            wire: b"\0\0\0\0\x02\0\xff",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let (client, server) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(
            hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(
                    hyper_util::rt::TokioIo::new(server),
                    handler::RapiraService::new(
                        shared(config(), &backend),
                        Addr::Inet(([127, 0, 0, 1], 41000).into()),
                        Addr::Inet(([127, 0, 0, 1], 9001).into()),
                    ),
                ),
        );
        let (mut client, connection) = h2::client::handshake(client).await.unwrap();
        let client_task = tokio::spawn(connection);
        for index in 0..128 {
            poll_fn(|cx| client.poll_ready(cx)).await.unwrap();
            let req = request("http://localhost/example.Echo/Call", case.wire).map(|_| ());
            let (response, mut upload) = client.send_request(req, false).unwrap();
            upload
                .send_data(Bytes::from_static(case.wire), true)
                .unwrap();
            let response = response
                .await
                .unwrap_or_else(|error| panic!("{} call {index}: {error}", case.name));
            let mut body = response.into_body();
            let mut wire = Vec::new();
            while let Some(data) = body.data().await {
                let data =
                    data.unwrap_or_else(|error| panic!("{} call {index}: {error}", case.name));
                wire.extend_from_slice(&data);
                body.flow_control().release_capacity(data.len()).unwrap();
            }
            assert_eq!(wire, case.wire, "{} call {index}", case.name);
            let trailers = body.trailers().await.unwrap().expect("status trailers");
            assert_eq!(trailers["grpc-status"], "0", "{} call {index}", case.name);
        }
        server_task.abort();
        client_task.abort();
    }
}

#[tokio::test]
async fn identity_response_deadline_covers_unconsumed_frames() {
    struct Case {
        name: &'static str,
        consume_prefix: bool,
    }
    let cases = [
        Case {
            name: "before_prefix",
            consume_prefix: false,
        },
        Case {
            name: "after_prefix",
            consume_prefix: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let shared = shared(config(), &backend);
        let mut req = request(
            "/example.Echo/Call",
            b"\0\0\0\0\x04\0\xff\x80\x01".as_slice(),
        );
        req.headers_mut()
            .insert("grpc-timeout", "1S".parse().unwrap());
        let mut body = call(Arc::clone(&shared), req).await.into_body();
        if case.consume_prefix {
            let prefix = body.frame().await.unwrap().unwrap().into_data().unwrap();
            assert_eq!(prefix.as_ref(), b"\0\0\0\0\x04", "{}", case.name);
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let trailers = body
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap();
        assert_eq!(trailers["grpc-status"], "4", "{}", case.name);
        assert!(body.frame().await.is_none(), "{}", case.name);
        assert_eq!(shared.inflight.load(Ordering::Acquire), 1, "{}", case.name);
        drop(body);
        assert_eq!(shared.inflight.load(Ordering::Acquire), 0, "{}", case.name);
    }
}

#[tokio::test]
async fn identity_response_deadline_survives_http2_flow_control() {
    struct Case {
        name: &'static str,
        wire: &'static [u8],
        prefix: &'static [u8; 5],
    }
    let cases = [
        Case {
            name: "binary_payload",
            wire: b"\0\0\0\0\x04\0\xff\x80\x01",
            prefix: b"\0\0\0\0\x04",
        },
        Case {
            name: "eight_byte_payload",
            wire: b"\0\0\0\0\x08\0\x01\x02\x03\x04\x05\x06\x07",
            prefix: b"\0\0\0\0\x08",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let (client, server) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(
            hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(
                    hyper_util::rt::TokioIo::new(server),
                    handler::RapiraService::new(
                        shared(config(), &backend),
                        Addr::Inet(([127, 0, 0, 1], 41000).into()),
                        Addr::Inet(([127, 0, 0, 1], 9001).into()),
                    ),
                ),
        );
        let (mut client, mut connection) = h2::client::Builder::new()
            .initial_window_size(0)
            .handshake(client)
            .await
            .unwrap();
        let (resume, paused) = tokio::sync::oneshot::channel();
        let client_task = tokio::spawn(async move {
            tokio::select! {
                result = &mut connection => return result,
                _ = paused => connection.set_initial_window_size(65535).unwrap(),
            }
            connection.await
        });
        let mut req = request("http://localhost/example.Echo/Call", case.wire).map(|_| ());
        req.headers_mut()
            .insert("grpc-timeout", "1S".parse().unwrap());
        let (response, mut upload) = client.send_request(req, false).unwrap();
        upload
            .send_data(Bytes::from_static(case.wire), true)
            .unwrap();
        let response = response.await.unwrap();
        let headers = response.headers().clone();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        resume.send(()).unwrap();
        let mut body = response.into_body();
        let mut wire = Vec::new();
        while let Some(data) = body.data().await {
            let data = data.unwrap_or_else(|error| panic!("{}: {error}", case.name));
            wire.extend_from_slice(&data);
        }
        let trailers = body.trailers().await.unwrap().expect("deadline trailers");
        assert_eq!(trailers["grpc-status"], "4", "{}", case.name);
        assert_eq!(wire, case.prefix, "{}", case.name);
        assert!(!headers.contains_key("content-length"), "{}", case.name);
        server_task.abort();
        client_task.abort();
    }
}

#[tokio::test]
async fn response_size_limits_preserve_a_single_message() {
    struct Case {
        name: &'static str,
        message: &'static [u8],
        code: tonic::Code,
        wire: &'static [u8],
    }
    let cases = [
        Case {
            name: "empty_response",
            message: b"",
            code: tonic::Code::Ok,
            wire: b"\0\0\0\0\0",
        },
        Case {
            name: "at_limit",
            message: b"ab",
            code: tonic::Code::Ok,
            wire: b"\0\0\0\0\x02ab",
        },
        Case {
            name: "over_limit",
            message: b"abc",
            code: tonic::Code::OutOfRange,
            wire: b"",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        backend.replies.lock().unwrap().push_back(Ok(grpc::Reply {
            headers: Vec::new(),
            trailers: Vec::new(),
            result: Ok(Bytes::from_static(case.message)),
        }));
        let cfg = Config {
            max_response_message_size: 2,
            ..config()
        };
        let (headers, body, trailers) = collect(
            call(
                shared(cfg, &backend),
                request("/example.Echo/Call", b"\0\0\0\0\0".as_slice()),
            )
            .await,
        )
        .await;
        assert_eq!(
            status(&headers, &trailers).code(),
            case.code,
            "{}",
            case.name
        );
        assert_eq!(body.as_ref(), case.wire, "{}", case.name);
    }
}

#[tokio::test]
async fn incoming_metadata_preserves_duplicates_and_decodes_binary_values() {
    let backend = Arc::new(TestBackend::default());
    let mut req = request("/example.Echo/Call", frame(b""));
    req.headers_mut().append("x-text", "first".parse().unwrap());
    req.headers_mut()
        .append("x-text", "second".parse().unwrap());
    req.headers_mut()
        .append("x-token-bin", "AP8=, YQ".parse().unwrap());
    req.headers_mut()
        .append("x-token-bin", "Yg==".parse().unwrap());
    req.headers_mut().insert(
        "x-nonascii",
        http::HeaderValue::from_bytes(b"\xff").unwrap(),
    );
    req.headers_mut()
        .insert("grpc-private", "hidden".parse().unwrap());
    req.headers_mut()
        .insert("content-custom", "hidden".parse().unwrap());
    let (headers, _, trailers) = collect(call(shared(config(), &backend), req).await).await;
    assert_eq!(status(&headers, &trailers).code(), tonic::Code::Ok);
    let seen = backend.seen.lock().unwrap();
    let fields = &seen[0].metadata;
    let get = |name| {
        fields
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_slice())
            .collect::<Vec<_>>()
    };
    assert_eq!(get("x-text"), [b"first".as_slice(), b"second".as_slice()]);
    assert_eq!(
        get("x-token-bin"),
        [b"\0\xff".as_slice(), b"a".as_slice(), b"b".as_slice()]
    );
    assert_eq!(fields.len(), 5);
}

#[derive(Clone, PartialEq, Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<prost_types::Any>,
}

#[tokio::test]
async fn initial_headers_and_final_trailers_remain_separate() {
    struct Case {
        name: &'static str,
        failure: bool,
    }
    let cases = [
        Case {
            name: "success",
            failure: false,
        },
        Case {
            name: "rich_failure",
            failure: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let headers: FieldLines = vec![
            ("x-stage".into(), b"head-one".to_vec()),
            ("x-stage".into(), b"head-two".to_vec()),
            ("x-token-bin".into(), vec![0, 255]),
        ];
        let trailers: FieldLines = vec![
            ("x-stage".into(), b"tail-one".to_vec()),
            ("x-stage".into(), b"tail-two".to_vec()),
            ("x-token-bin".into(), vec![255, 0]),
        ];
        backend.replies.lock().unwrap().push_back(Ok(grpc::Reply {
            headers,
            trailers,
            result: if case.failure {
                Err(grpc::Status {
                    code: 8,
                    message: "limit: 50% ☃".into(),
                    details: vec![grpc::ErrorDetail {
                        type_url: "type.googleapis.com/example.Detail".into(),
                        value: Bytes::from_static(b"\x08\x07"),
                    }],
                })
            } else {
                Ok(Bytes::new())
            },
        }));
        let (headers, body, trailers) = collect(
            call(
                shared(config(), &backend),
                request("/example.Echo/Call", frame(b"")),
            )
            .await,
        )
        .await;
        assert_eq!(
            values(&headers, "x-stage"),
            [b"head-one".to_vec(), b"head-two".to_vec()],
            "{}",
            case.name
        );
        assert_eq!(
            values(&trailers, "x-stage"),
            [b"tail-one".to_vec(), b"tail-two".to_vec()],
            "{}",
            case.name
        );
        assert_eq!(headers["x-token-bin"], "AP8", "{}", case.name);
        assert_eq!(trailers["x-token-bin"], "/wA", "{}", case.name);
        assert!(headers.get("grpc-status").is_none(), "{}", case.name);
        let status = status(&headers, &trailers);
        if case.failure {
            assert_eq!(status.code(), tonic::Code::ResourceExhausted);
            assert_eq!(status.message(), "limit: 50% ☃");
            let rich = RpcStatus::decode(status.details()).unwrap();
            assert_eq!(rich.code, 8);
            assert_eq!(rich.message, "limit: 50% ☃");
            assert_eq!(
                rich.details,
                [prost_types::Any {
                    type_url: "type.googleapis.com/example.Detail".into(),
                    value: vec![8, 7]
                }]
            );
            assert!(body.is_empty());
        } else {
            assert_eq!(status.code(), tonic::Code::Ok);
            assert_eq!(body.as_ref(), b"\0\0\0\0\0");
        }
    }
}

#[tokio::test]
async fn route_and_transport_validation_precede_php() {
    struct Case {
        name: &'static str,
        path: &'static str,
        method: &'static str,
        content_type: &'static str,
        http: u16,
        grpc: Option<tonic::Code>,
    }
    let cases = [
        Case {
            name: "unknown_service",
            path: "/missing.Service/Call",
            method: "POST",
            content_type: "application/grpc",
            http: 200,
            grpc: Some(tonic::Code::Unimplemented),
        },
        Case {
            name: "unknown_method",
            path: "/example.Echo/Missing",
            method: "POST",
            content_type: "application/grpc",
            http: 200,
            grpc: Some(tonic::Code::Unimplemented),
        },
        Case {
            name: "wrong_method",
            path: "/example.Echo/Call",
            method: "GET",
            content_type: "application/grpc",
            http: 405,
            grpc: None,
        },
        Case {
            name: "json_is_unsupported",
            path: "/example.Echo/Call",
            method: "POST",
            content_type: "application/grpc+json",
            http: 415,
            grpc: None,
        },
        Case {
            name: "grpc_web_is_unsupported",
            path: "/example.Echo/Call",
            method: "POST",
            content_type: "application/grpc-web",
            http: 415,
            grpc: None,
        },
        Case {
            name: "native_proto",
            path: "/example.Echo/Call",
            method: "POST",
            content_type: "application/grpc+proto",
            http: 200,
            grpc: Some(tonic::Code::Ok),
        },
        Case {
            name: "native_grpc_with_parameters",
            path: "/example.Echo/Call",
            method: "POST",
            content_type: "application/grpc;charset=utf-8",
            http: 200,
            grpc: Some(tonic::Code::Ok),
        },
        Case {
            name: "native_proto_with_parameters",
            path: "/example.Echo/Call",
            method: "POST",
            content_type: "application/grpc+proto;q=1",
            http: 200,
            grpc: Some(tonic::Code::Ok),
        },
        Case {
            name: "native_proto_with_parameter_whitespace",
            path: "/example.Echo/Call",
            method: "POST",
            content_type: "application/grpc+proto \t; charset=utf-8",
            http: 200,
            grpc: Some(tonic::Code::Ok),
        },
        Case {
            name: "grpc_web_with_parameters_is_unsupported",
            path: "/example.Echo/Call",
            method: "POST",
            content_type: "application/grpc-web;charset=utf-8",
            http: 415,
            grpc: None,
        },
        Case {
            name: "unknown_proto_suffix_with_parameters_is_unsupported",
            path: "/example.Echo/Call",
            method: "POST",
            content_type: "application/grpc+protobuf;charset=utf-8",
            http: 415,
            grpc: None,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut req = request(case.path, frame(b""));
        *req.method_mut() = case.method.parse().unwrap();
        req.headers_mut()
            .insert("content-type", case.content_type.parse().unwrap());
        let res = call(shared(config(), &backend), req).await;
        assert_eq!(res.status().as_u16(), case.http, "{}", case.name);
        if let Some(code) = case.grpc {
            let (headers, _, trailers) = collect(res).await;
            assert_eq!(status(&headers, &trailers).code(), code, "{}", case.name);
        }
        assert_eq!(
            backend.seen.lock().unwrap().len(),
            usize::from(case.grpc == Some(tonic::Code::Ok)),
            "{}",
            case.name
        );
    }
}

struct Tag(&'static str);
impl Middleware for Tag {
    fn handle<'a>(&'a self, mut req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
        Box::pin(async move {
            assert_eq!(req.extensions().get::<Protocol>(), Some(&Protocol::Grpc));
            assert!(req.extensions().get::<Peer>().unwrap().received_at > 0.0);
            req.headers_mut()
                .append("x-trace", format!("{}-in", self.0).parse().unwrap());
            let mut res = next.run(req).await;
            res.headers_mut()
                .append("x-trace", format!("{}-out", self.0).parse().unwrap());
            res
        })
    }
}

struct Deny(Rejection);
impl Middleware for Deny {
    fn handle<'a>(&'a self, _: HttpRequest, _: Next) -> BoxFuture<'a, HttpResponse> {
        Box::pin(async { self.0.into_response() })
    }
}

#[tokio::test]
async fn interceptor_order_and_shared_rejections() {
    struct Case {
        name: &'static str,
        rejection: Option<Rejection>,
        code: tonic::Code,
    }
    let cases = [
        Case {
            name: "passes",
            rejection: None,
            code: tonic::Code::Ok,
        },
        Case {
            name: "authentication_required",
            rejection: Some(Rejection::AuthenticationRequired),
            code: tonic::Code::Unauthenticated,
        },
        Case {
            name: "access_denied",
            rejection: Some(Rejection::AccessDenied),
            code: tonic::Code::PermissionDenied,
        },
        Case {
            name: "rate_limited",
            rejection: Some(Rejection::RateLimited),
            code: tonic::Code::ResourceExhausted,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut cfg = config();
        cfg.interceptors = vec![Arc::new(Tag("a")), Arc::new(Tag("b"))];
        if let Some(rejection) = case.rejection {
            cfg.interceptors.push(Arc::new(Deny(rejection)));
        }
        let (headers, _, trailers) = collect(
            call(
                shared(cfg, &backend),
                request("/example.Echo/Call", frame(b"")),
            )
            .await,
        )
        .await;
        assert_eq!(
            status(&headers, &trailers).code(),
            case.code,
            "{}",
            case.name
        );
        assert_eq!(
            values(&headers, "x-trace"),
            [b"b-out".to_vec(), b"a-out".to_vec()],
            "{}",
            case.name
        );
        let seen = backend.seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            usize::from(case.rejection.is_none()),
            "{}",
            case.name
        );
        if case.rejection.is_none() {
            assert_eq!(
                seen[0]
                    .metadata
                    .iter()
                    .filter(|(key, _)| key == "x-trace")
                    .map(|(_, value)| value.as_slice())
                    .collect::<Vec<_>>(),
                [b"a-in".as_slice(), b"b-in".as_slice()]
            );
        }
    }
}

struct Delay;
impl Middleware for Delay {
    fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            next.run(req).await
        })
    }
}

#[tokio::test]
async fn deadline_covers_interceptors_and_drops_backend_waits() {
    struct Case {
        name: &'static str,
        delay: bool,
        dispatched: bool,
    }
    let cases = [
        Case {
            name: "expires_in_interceptor",
            delay: true,
            dispatched: false,
        },
        Case {
            name: "expires_waiting_for_php",
            delay: false,
            dispatched: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend {
            hold: true,
            ..Default::default()
        });
        let mut cfg = config();
        if case.delay {
            cfg.interceptors.push(Arc::new(Delay));
        }
        let mut req = request("/example.Echo/Call", frame(b""));
        req.headers_mut()
            .insert("grpc-timeout", "20m".parse().unwrap());
        let (headers, _, trailers) = collect(call(shared(cfg, &backend), req).await).await;
        assert_eq!(
            status(&headers, &trailers).code(),
            tonic::Code::DeadlineExceeded,
            "{}",
            case.name
        );
        let seen = backend.seen.lock().unwrap();
        assert_eq!(seen.len(), usize::from(case.dispatched), "{}", case.name);
        assert_eq!(
            backend.dropped.load(Ordering::Acquire),
            case.dispatched,
            "{}",
            case.name
        );
        if case.dispatched {
            assert!((seen[0].deadline.unwrap() - seen[0].received_at - 0.02).abs() < 0.00001);
            assert!(seen[0].expires_at.is_some());
        }
    }
}

#[tokio::test]
async fn backend_errors_are_sanitized() {
    struct Case {
        name: &'static str,
        rejection: Option<u16>,
        code: tonic::Code,
    }
    let cases = [
        Case {
            name: "unexpected_host_failure",
            rejection: None,
            code: tonic::Code::Internal,
        },
        Case {
            name: "worker_saturated",
            rejection: Some(429),
            code: tonic::Code::ResourceExhausted,
        },
        Case {
            name: "worker_unavailable",
            rejection: Some(503),
            code: tonic::Code::Unavailable,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let error = case.rejection.map_or_else(
            || anyhow::anyhow!("private backend credentials"),
            |status| {
                extension_api::Rejected {
                    status,
                    reason: "private backend credentials".into(),
                }
                .into()
            },
        );
        backend.replies.lock().unwrap().push_back(Err(error));
        let (headers, body, trailers) = collect(
            call(
                shared(config(), &backend),
                request("/example.Echo/Call", frame(b"")),
            )
            .await,
        )
        .await;
        let status = status(&headers, &trailers);
        assert_eq!(status.code(), case.code, "{}", case.name);
        assert!(!status.message().contains("private"), "{}", case.name);
        assert!(status.details().is_empty(), "{}", case.name);
        assert!(body.is_empty(), "{}", case.name);
    }
}

async fn reflect(
    shared: Arc<handler::Shared>,
    version: &str,
    query: pb::server_reflection_request::MessageRequest,
) -> pb::ServerReflectionResponse {
    let req = pb::ServerReflectionRequest {
        host: String::new(),
        message_request: Some(query),
    };
    let (headers, body, trailers) = collect(
        call(
            shared,
            request(
                &format!("/grpc.reflection.{version}.ServerReflection/ServerReflectionInfo"),
                frame(&req.encode_to_vec()),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(status(&headers, &trailers).code(), tonic::Code::Ok);
    assert_eq!(body[0], 0);
    assert_eq!(
        u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize,
        body.len() - 5
    );
    pb::ServerReflectionResponse::decode(&body[5..]).unwrap()
}

#[tokio::test]
async fn reflection_versions_filter_services_and_preserve_descriptors() {
    use pb::server_reflection_request::MessageRequest;
    use pb::server_reflection_response::MessageResponse;
    struct Case {
        name: &'static str,
        version: &'static str,
    }
    let cases = [
        Case {
            name: "stable",
            version: "v1",
        },
        Case {
            name: "alpha",
            version: "v1alpha",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let shared = shared(config(), &backend);
        let response = reflect(
            Arc::clone(&shared),
            case.version,
            MessageRequest::ListServices(String::new()),
        )
        .await;
        let Some(MessageResponse::ListServicesResponse(list)) = response.message_response else {
            panic!("{}: service list", case.name);
        };
        let mut names: Vec<_> = list
            .service
            .into_iter()
            .map(|service| service.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "example.Echo",
                "grpc.reflection.v1.ServerReflection",
                "grpc.reflection.v1alpha.ServerReflection"
            ],
            "{}",
            case.name
        );
        let response = reflect(
            Arc::clone(&shared),
            case.version,
            MessageRequest::FileByFilename("vendor.proto".into()),
        )
        .await;
        let Some(MessageResponse::FileDescriptorResponse(files)) = response.message_response else {
            panic!("{}: imported descriptor", case.name);
        };
        assert_eq!(
            prost_types::FileDescriptorProto::decode(files.file_descriptor_proto[0].as_slice())
                .unwrap()
                .name(),
            "vendor.proto"
        );
        let response = reflect(
            Arc::clone(&shared),
            case.version,
            MessageRequest::FileByFilename("google/protobuf/descriptor.proto".into()),
        )
        .await;
        let Some(MessageResponse::FileDescriptorResponse(files)) = response.message_response else {
            panic!("{}: transitive descriptor", case.name);
        };
        assert_eq!(
            prost_types::FileDescriptorProto::decode(files.file_descriptor_proto[0].as_slice())
                .unwrap()
                .name(),
            "google/protobuf/descriptor.proto"
        );
        let response = reflect(
            shared,
            case.version,
            MessageRequest::FileContainingSymbol("example.Echo.Call".into()),
        )
        .await;
        let Some(MessageResponse::FileDescriptorResponse(files)) = response.message_response else {
            panic!("{}: method descriptor", case.name);
        };
        assert!(
            files.file_descriptor_proto[0]
                .windows(8)
                .any(|bytes| bytes == b"\x8a\xb5\x18\x04kept"),
            "{}: custom option",
            case.name
        );
        assert!(backend.seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn reflection_runs_while_php_is_busy_and_uses_interceptors() {
    use pb::server_reflection_request::MessageRequest;
    let backend = Arc::new(TestBackend {
        hold: true,
        ..Default::default()
    });
    let busy_shared = shared(config(), &backend);
    let app = tokio::spawn(call(
        Arc::clone(&busy_shared),
        request("/example.Echo/Call", frame(b"")),
    ));
    backend.started.notified().await;
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        reflect(
            busy_shared,
            "v1",
            MessageRequest::ListServices(String::new()),
        ),
    )
    .await
    .unwrap();
    assert!(response.message_response.is_some());
    app.abort();
    assert!(app.await.unwrap_err().is_cancelled());
    assert!(backend.dropped.load(Ordering::Acquire));
    let cfg = Config {
        interceptors: vec![Arc::new(Deny(Rejection::AccessDenied))],
        ..config()
    };
    let (headers, _, trailers) = collect(
        call(
            shared(cfg, &backend),
            request(
                "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo",
                frame(b""),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(
        status(&headers, &trailers).code(),
        tonic::Code::PermissionDenied
    );
}

#[tokio::test]
async fn shutdown_joins_after_run_is_cancelled() {
    use extension_api::Extension;
    let dir = tempfile::tempdir().unwrap();
    struct Case {
        name: &'static str,
        listen: extension_api::ListenAddr,
    }
    let cases = [
        Case {
            name: "tcp",
            listen: extension_api::ListenAddr::Tcp(([127, 0, 0, 1], 0).into()),
        },
        Case {
            name: "unix",
            listen: extension_api::ListenAddr::Unix(dir.path().join("grpc.sock")),
        },
    ];
    for case in cases {
        let mut server = Server::init(Config {
            listen: case.listen,
            ..config()
        });
        server
            .prepare(&mut extension_api::PrepareCtx::new())
            .unwrap();
        let backend = Arc::new(TestBackend::default());
        let mut run = Box::pin(server.run(Php::new(backend.clone())));
        assert!(
            poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx).is_pending())).await,
            "{}",
            case.name
        );
        drop(run);
        assert!(server.join.is_some(), "{}", case.name);
        server.shutdown().await.unwrap();
        assert!(server.join.is_none(), "{}", case.name);
        assert_eq!(Arc::strong_count(&backend), 1, "{}", case.name);
    }
}

#[tokio::test]
async fn stream_reset_drops_the_pending_backend_future() {
    let backend = Arc::new(TestBackend {
        hold: true,
        ..Default::default()
    });
    let shared = shared(config(), &backend);
    let (client, server) = tokio::io::duplex(65536);
    let connection = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection(
            hyper_util::rt::TokioIo::new(server),
            handler::RapiraService::new(
                shared,
                Addr::Inet(([127, 0, 0, 1], 41000).into()),
                Addr::Inet(([127, 0, 0, 1], 9001).into()),
            ),
        );
    let server_task = tokio::spawn(connection);
    let (mut client, connection) = h2::client::handshake(client).await.unwrap();
    let client_task = tokio::spawn(connection);
    let req = request("http://localhost/example.Echo/Call", frame(b"")).map(|_| ());
    let (response, mut upload) = client.send_request(req, false).unwrap();
    upload
        .send_data(Bytes::from_static(b"\0\0\0\0\0"), true)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), backend.started.notified())
        .await
        .unwrap();
    upload.send_reset(h2::Reason::CANCEL);
    assert!(response.await.is_err());
    tokio::time::timeout(Duration::from_secs(1), async {
        while !backend.dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("RST_STREAM must cancel the backend wait");
    server_task.abort();
    client_task.abort();
}

struct StalledBody(Option<Bytes>);
impl Body for StalledBody {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match self.0.take() {
            Some(bytes) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            None => Poll::Pending,
        }
    }
}

#[tokio::test]
async fn deadline_covers_upload_and_reflection_streams() {
    struct Case {
        name: &'static str,
        path: &'static str,
        data: &'static [u8],
    }
    let cases = [
        Case {
            name: "partial_prefix",
            path: "/example.Echo/Call",
            data: b"\0\0",
        },
        Case {
            name: "missing_upload_end",
            path: "/example.Echo/Call",
            data: b"\0\0\0\0\0",
        },
        Case {
            name: "reflection_waiting_for_input",
            path: "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
            data: b"",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut req = request(case.path, b"".as_slice())
            .map(|_| StalledBody(Some(Bytes::from_static(case.data))).boxed_unsync());
        req.headers_mut()
            .insert("grpc-timeout", "10m".parse().unwrap());
        let (headers, _, trailers) = collect(call(shared(config(), &backend), req).await).await;
        assert_eq!(
            status(&headers, &trailers).code(),
            tonic::Code::DeadlineExceeded,
            "{}",
            case.name
        );
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
    }
}

#[tokio::test]
async fn fragmented_unary_input_rejects_incomplete_trailing_data() {
    struct Case {
        name: &'static str,
        chunks: &'static [&'static [u8]],
        trailers: bool,
        code: tonic::Code,
    }
    let cases = [
        Case {
            name: "split_prefix_and_payload",
            chunks: &[b"\0\0", b"\0\0\x02a", b"b"],
            trailers: false,
            code: tonic::Code::Ok,
        },
        Case {
            name: "empty_data_frame",
            chunks: &[b"\0\0\0\0\0", b""],
            trailers: false,
            code: tonic::Code::Ok,
        },
        Case {
            name: "partial_suffix_before_trailers",
            chunks: &[b"\0\0\0\0\0", b"\0\0"],
            trailers: true,
            code: tonic::Code::Internal,
        },
        Case {
            name: "missing_second_payload_before_trailers",
            chunks: &[b"\0\0\0\0\0", b"\0\0\0\0\x01"],
            trailers: true,
            code: tonic::Code::Internal,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut frames: Vec<Result<Frame<Bytes>, BoxError>> = case
            .chunks
            .iter()
            .map(|chunk| Ok(Frame::data(Bytes::from_static(chunk))))
            .collect();
        if case.trailers {
            frames.push(Ok(Frame::trailers(HeaderMap::new())));
        }
        let req = request("/example.Echo/Call", b"".as_slice())
            .map(|_| StreamBody::new(tokio_stream::iter(frames)).boxed_unsync());
        let (headers, _, trailers) = collect(call(shared(config(), &backend), req).await).await;
        assert_eq!(
            status(&headers, &trailers).code(),
            case.code,
            "{}",
            case.name
        );
        assert_eq!(
            backend.seen.lock().unwrap().len(),
            usize::from(case.code == tonic::Code::Ok),
            "{}",
            case.name
        );
    }
}

#[tokio::test]
async fn response_compression_requires_server_opt_in() {
    struct Case {
        name: &'static str,
        accept: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "no_accept_header",
            accept: None,
        },
        Case {
            name: "gzip_accepted",
            accept: Some("gzip"),
        },
        Case {
            name: "gzip_and_identity_accepted",
            accept: Some("gzip,identity"),
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let message = b"\x0a\x02\0\xff";
        let mut req = request("/example.Echo/Call", frame(message));
        if let Some(accept) = case.accept {
            req.headers_mut()
                .insert("grpc-accept-encoding", accept.parse().unwrap());
        }
        let (headers, body, trailers) = collect(call(shared(config(), &backend), req).await).await;
        assert_eq!(
            status(&headers, &trailers).code(),
            tonic::Code::Ok,
            "{}",
            case.name
        );
        assert!(headers.get("grpc-encoding").is_none(), "{}", case.name);
        assert_eq!(
            body.as_ref(),
            b"\0\0\0\0\x04\x0a\x02\0\xff",
            "{}",
            case.name
        );
    }
}

#[tokio::test]
async fn compression_uses_tonic_and_checks_uncompressed_message_limits() {
    use std::io::{Read, Write};
    struct Case {
        name: &'static str,
        gzip_responses: bool,
        accept: Option<&'static str>,
        response_gzip: bool,
        request_limit: usize,
        response_limit: usize,
        code: tonic::Code,
    }
    let cases = [
        Case {
            name: "gzip_round_trip",
            gzip_responses: true,
            accept: Some("gzip"),
            response_gzip: true,
            request_limit: 128,
            response_limit: 128,
            code: tonic::Code::Ok,
        },
        Case {
            name: "gzip_request_with_response_gzip_disabled",
            gzip_responses: false,
            accept: Some("gzip"),
            response_gzip: false,
            request_limit: 128,
            response_limit: 128,
            code: tonic::Code::Ok,
        },
        Case {
            name: "opted_in_with_identity_only_client",
            gzip_responses: true,
            accept: Some("identity"),
            response_gzip: false,
            request_limit: 128,
            response_limit: 128,
            code: tonic::Code::Ok,
        },
        Case {
            name: "opted_in_without_accept_header",
            gzip_responses: true,
            accept: None,
            response_gzip: false,
            request_limit: 128,
            response_limit: 128,
            code: tonic::Code::Ok,
        },
        Case {
            name: "decompressed_request_limit",
            gzip_responses: true,
            accept: Some("gzip"),
            response_gzip: true,
            request_limit: 64,
            response_limit: 128,
            code: tonic::Code::ResourceExhausted,
        },
        Case {
            name: "uncompressed_response_limit",
            gzip_responses: true,
            accept: Some("gzip"),
            response_gzip: true,
            request_limit: 128,
            response_limit: 64,
            code: tonic::Code::OutOfRange,
        },
        Case {
            name: "decompressed_request_limit_with_response_gzip_disabled",
            gzip_responses: false,
            accept: Some("gzip"),
            response_gzip: false,
            request_limit: 64,
            response_limit: 128,
            code: tonic::Code::ResourceExhausted,
        },
        Case {
            name: "response_limit_with_response_gzip_disabled",
            gzip_responses: false,
            accept: Some("gzip"),
            response_gzip: false,
            request_limit: 128,
            response_limit: 64,
            code: tonic::Code::OutOfRange,
        },
    ];
    let message = [b'a'; 128];
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&message).unwrap();
    let mut wire = frame(&gzip.finish().unwrap());
    wire[0] = 1;
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let cfg = Config {
            gzip_responses: case.gzip_responses,
            max_request_message_size: case.request_limit,
            max_response_message_size: case.response_limit,
            ..config()
        };
        let mut req = request("/example.Echo/Call", wire.clone());
        req.headers_mut()
            .insert("grpc-encoding", "gzip".parse().unwrap());
        if let Some(accept) = case.accept {
            req.headers_mut()
                .insert("grpc-accept-encoding", accept.parse().unwrap());
        }
        let (headers, body, trailers) = collect(call(shared(cfg, &backend), req).await).await;
        assert_eq!(
            status(&headers, &trailers).code(),
            case.code,
            "{}",
            case.name
        );
        if case.code == tonic::Code::Ok {
            if case.response_gzip {
                assert_eq!(headers["grpc-encoding"], "gzip", "{}", case.name);
                assert_eq!(body[0], 1, "{}", case.name);
                assert_eq!(
                    u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize,
                    body.len() - 5,
                    "{}",
                    case.name
                );
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(&body[5..])
                    .read_to_end(&mut decoded)
                    .unwrap();
                assert_eq!(decoded, message, "{}", case.name);
            } else {
                assert!(headers.get("grpc-encoding").is_none(), "{}", case.name);
                assert_eq!(body.as_ref(), frame(&message).as_slice(), "{}", case.name);
            }
        } else {
            assert!(body.is_empty(), "{}", case.name);
        }
    }
}

#[tokio::test]
async fn shutdown_drains_active_calls_within_its_grace_period() {
    use extension_api::{Extension, ListenAddr};
    struct Case {
        name: &'static str,
        finish: bool,
        grace: Duration,
    }
    let cases = [
        Case {
            name: "reply_finishes_during_drain",
            finish: true,
            grace: Duration::from_secs(1),
        },
        Case {
            name: "grace_expiry_cancels_php",
            finish: false,
            grace: Duration::from_millis(20),
        },
    ];
    for case in cases {
        let mut server = Server::init(Config {
            drain_grace: case.grace,
            ..config()
        });
        server
            .prepare(&mut extension_api::PrepareCtx::new())
            .unwrap();
        let ListenAddr::Tcp(addr) = *server.prepared.as_ref().unwrap().addr() else {
            unreachable!();
        };
        let backend = Arc::new(TestBackend {
            hold: true,
            ..Default::default()
        });
        let mut run = Box::pin(server.run(Php::new(backend.clone())));
        assert!(poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx).is_pending())).await);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut client, connection) = h2::client::handshake(stream).await.unwrap();
        let connection = tokio::spawn(connection);
        let (response, mut upload) = client
            .send_request(
                request("http://localhost/example.Echo/Call", frame(b"")).map(|_| ()),
                false,
            )
            .unwrap();
        upload
            .send_data(Bytes::from_static(b"\0\0\0\0\0"), true)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), backend.started.notified())
            .await
            .unwrap();
        drop(run);
        let release = if case.finish {
            let backend = Arc::clone(&backend);
            Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                backend.release.notify_one();
            }))
        } else {
            None
        };
        let result = tokio::time::timeout(Duration::from_secs(2), server.shutdown())
            .await
            .unwrap();
        assert_eq!(result.is_ok(), case.finish, "{}: {result:?}", case.name);
        if let Some(release) = release {
            release.await.unwrap();
        }
        assert!(backend.dropped.load(Ordering::Acquire), "{}", case.name);
        assert_eq!(Arc::strong_count(&backend), 1, "{}", case.name);
        if case.finish {
            let mut response = response.await.unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            assert_eq!(
                response.body_mut().data().await.unwrap().unwrap().as_ref(),
                b"\0\0\0\0\0"
            );
            assert_eq!(
                response.body_mut().trailers().await.unwrap().unwrap()["grpc-status"],
                "0"
            );
        } else {
            assert!(response.await.is_err(), "{}", case.name);
        }
        connection.abort();
    }
}

#[test]
fn timeout_units_and_validation() {
    struct Case {
        name: &'static str,
        header: Option<&'static str>,
        seconds: Option<f64>,
        valid: bool,
    }
    let cases = [
        Case {
            name: "no_deadline",
            header: None,
            seconds: None,
            valid: true,
        },
        Case {
            name: "hours",
            header: Some("2H"),
            seconds: Some(7200.0),
            valid: true,
        },
        Case {
            name: "minutes",
            header: Some("3M"),
            seconds: Some(180.0),
            valid: true,
        },
        Case {
            name: "seconds",
            header: Some("4S"),
            seconds: Some(4.0),
            valid: true,
        },
        Case {
            name: "milliseconds",
            header: Some("5m"),
            seconds: Some(0.005),
            valid: true,
        },
        Case {
            name: "microseconds",
            header: Some("6u"),
            seconds: Some(0.000006),
            valid: true,
        },
        Case {
            name: "nanoseconds",
            header: Some("7n"),
            seconds: Some(0.000000007),
            valid: true,
        },
        Case {
            name: "zero_expires_now",
            header: Some("0n"),
            seconds: Some(0.0),
            valid: true,
        },
        Case {
            name: "eight_digits",
            header: Some("99999999S"),
            seconds: Some(99999999.0),
            valid: true,
        },
        Case {
            name: "nine_digits",
            header: Some("123456789n"),
            seconds: None,
            valid: false,
        },
        Case {
            name: "signed_value",
            header: Some("+1S"),
            seconds: None,
            valid: false,
        },
        Case {
            name: "missing_digits",
            header: Some("S"),
            seconds: None,
            valid: false,
        },
        Case {
            name: "unknown_unit",
            header: Some("1h"),
            seconds: None,
            valid: false,
        },
        Case {
            name: "empty_value",
            header: Some(""),
            seconds: None,
            valid: false,
        },
    ];
    let now = std::time::Instant::now();
    for case in cases {
        let mut headers = HeaderMap::new();
        if let Some(value) = case.header {
            headers.insert("grpc-timeout", value.parse().unwrap());
        }
        let result = crate::deadline::parse(&headers, now, 10.0);
        assert_eq!(result.is_ok(), case.valid, "{}", case.name);
        if case.valid {
            let deadline = result.unwrap();
            assert_eq!(deadline.is_some(), case.seconds.is_some(), "{}", case.name);
            if let Some(deadline) = deadline {
                let seconds = case.seconds.unwrap();
                assert!(
                    (deadline.expires_at.duration_since(now).as_secs_f64() - seconds).abs() < 1e-10,
                    "{}",
                    case.name
                );
                assert!(
                    (deadline.unix - 10.0 - seconds).abs() < 1e-10,
                    "{}",
                    case.name
                );
            }
        } else {
            assert_eq!(
                result.err().unwrap().code(),
                tonic::Code::InvalidArgument,
                "{}",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn invalid_metadata_and_deadlines_stop_dispatch() {
    struct Case {
        name: &'static str,
        headers: &'static [(&'static str, &'static str)],
        code: tonic::Code,
    }
    let cases = [
        Case {
            name: "invalid_binary_value",
            headers: &[("x-token-bin", "??")],
            code: tonic::Code::InvalidArgument,
        },
        Case {
            name: "invalid_timeout",
            headers: &[("grpc-timeout", "-1S")],
            code: tonic::Code::InvalidArgument,
        },
        Case {
            name: "repeated_timeout",
            headers: &[("grpc-timeout", "1S"), ("grpc-timeout", "2S")],
            code: tonic::Code::InvalidArgument,
        },
        Case {
            name: "expired_at_receipt",
            headers: &[("grpc-timeout", "0n")],
            code: tonic::Code::DeadlineExceeded,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut req = request("/example.Echo/Call", frame(b""));
        for &(name, value) in case.headers {
            req.headers_mut().append(name, value.parse().unwrap());
        }
        let (headers, _, trailers) = collect(call(shared(config(), &backend), req).await).await;
        assert_eq!(
            status(&headers, &trailers).code(),
            case.code,
            "{}",
            case.name
        );
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
    }
}

#[tokio::test]
async fn response_metadata_and_status_validation_protects_transport_fields() {
    struct Case {
        name: &'static str,
        field: (&'static str, &'static [u8]),
        failure: Option<u8>,
        code: tonic::Code,
    }
    let cases = [
        Case {
            name: "reserved_status_is_filtered",
            field: ("grpc-status", b"8"),
            failure: None,
            code: tonic::Code::Ok,
        },
        Case {
            name: "reserved_length_is_filtered",
            field: ("content-length", b"999"),
            failure: None,
            code: tonic::Code::Ok,
        },
        Case {
            name: "http_only_name_is_invalid",
            field: ("x!field", b"value"),
            failure: None,
            code: tonic::Code::Internal,
        },
        Case {
            name: "nonascii_text_is_invalid",
            field: ("x-field", b"\xff"),
            failure: None,
            code: tonic::Code::Internal,
        },
        Case {
            name: "failure_cannot_have_ok_code",
            field: ("x-field", b"value"),
            failure: Some(0),
            code: tonic::Code::Internal,
        },
        Case {
            name: "failure_code_out_of_range",
            field: ("x-field", b"value"),
            failure: Some(17),
            code: tonic::Code::Internal,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        backend.replies.lock().unwrap().push_back(Ok(grpc::Reply {
            headers: vec![(case.field.0.into(), case.field.1.to_vec())],
            trailers: vec![("grpc-status".into(), b"0".to_vec())],
            result: case.failure.map_or_else(
                || Ok(Bytes::new()),
                |code| {
                    Err(grpc::Status {
                        code,
                        message: "private invalid status".into(),
                        details: Vec::new(),
                    })
                },
            ),
        }));
        let (headers, _, trailers) = collect(
            call(
                shared(config(), &backend),
                request("/example.Echo/Call", frame(b"")),
            )
            .await,
        )
        .await;
        let status = status(&headers, &trailers);
        assert_eq!(status.code(), case.code, "{}", case.name);
        assert!(!status.message().contains("private"), "{}", case.name);
        assert!(headers.get("content-length").is_none(), "{}", case.name);
    }
}

#[tokio::test]
async fn disabled_reflection_routes_return_unimplemented() {
    struct Case {
        name: &'static str,
        path: &'static str,
    }
    let cases = [
        Case {
            name: "stable",
            path: "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
        },
        Case {
            name: "alpha",
            path: "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let cfg = Config {
            reflection: false,
            ..config()
        };
        let (headers, _, trailers) =
            collect(call(shared(cfg, &backend), request(case.path, frame(b""))).await).await;
        assert_eq!(
            status(&headers, &trailers).code(),
            tonic::Code::Unimplemented,
            "{}",
            case.name
        );
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
    }
}
