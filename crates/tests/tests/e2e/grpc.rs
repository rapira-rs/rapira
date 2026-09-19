use std::collections::BTreeSet;
use std::future::Future;
use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes};
use prost::Message;
use prost_types::FileDescriptorSet;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};
use tonic_reflection::pb::v1::{
    ServerReflectionRequest, server_reflection_client::ServerReflectionClient,
    server_reflection_request::MessageRequest, server_reflection_response::MessageResponse,
};

use crate::harness::{self, BOOT, Server};

#[derive(Default)]
struct RawCodec;

impl Codec for RawCodec {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = Self;
    type Decoder = Self;

    fn encoder(&mut self) -> Self::Encoder {
        Self
    }

    fn decoder(&mut self) -> Self::Decoder {
        Self
    }
}

impl Encoder for RawCodec {
    type Item = Bytes;
    type Error = Status;

    fn encode(&mut self, item: Bytes, buffer: &mut EncodeBuf<'_>) -> Result<(), Status> {
        buffer.put(item);
        Ok(())
    }
}

impl Decoder for RawCodec {
    type Item = Bytes;
    type Error = Status;

    fn decode(&mut self, buffer: &mut DecodeBuf<'_>) -> Result<Option<Bytes>, Status> {
        Ok(Some(buffer.copy_to_bytes(buffer.remaining())))
    }
}

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve test port")
        .local_addr()
        .expect("local address")
}

fn spawn(pool_extra: &str, combined_http: Option<SocketAddr>) -> Server {
    spawn_schema(
        pool_extra,
        combined_http,
        include_str!("fixtures/grpc/proto/echo.proto"),
    )
}

fn spawn_schema(pool_extra: &str, combined_http: Option<SocketAddr>, schema: &str) -> Server {
    let dir = harness::scratch_dir();
    std::fs::create_dir(dir.join("proto")).expect("create proto dir");
    std::fs::write(dir.join("proto/echo.proto"), schema).expect("write proto");
    std::fs::copy(
        harness::fixture_path("grpc/dispatcher.php"),
        dir.join("grpc.php"),
    )
    .expect("copy dispatcher");
    let addr = free_addr();
    let mut config = format!(
        "[grpc]\nlisten = '{addr}'\nprotos = ['proto']\n[grpc.pool]\nentrypoint = 'grpc.php'\nprocesses = 1\n{pool_extra}\n[supervisor]\nprocess_control_timeout_secs = 5\n"
    );
    if let Some(http) = combined_http {
        std::fs::write(
            dir.join("http.php"),
            "<?php $d = \\Rapira\\get_dispatcher(); try { while (true) { $e = $d->receive(); $e->writeBody('http-ok'); } } catch (\\Rapira\\Exception\\ClosedException) {}",
        )
        .expect("write HTTP dispatcher");
        config.push_str(&format!(
            "[http]\nlisten = '{http}'\n[http.pool]\nentrypoint = 'http.php'\nmode = 'dispatcher'\nprocesses = 1\n"
        ));
    }
    harness::spawn_staged_config(dir, addr, &config, &[("PATH", "")])
}

async fn connect(server: &mut Server) -> Channel {
    let endpoint = Endpoint::from_shared(format!("http://{}", server.addr))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(10));
    let end = tokio::time::Instant::now() + BOOT;
    loop {
        if let Some(status) = server.try_status() {
            panic!("server exited: {status}\n{}", harness::diagnostics(server));
        }
        match endpoint.connect().await {
            Ok(channel) => return channel,
            Err(error) if tokio::time::Instant::now() >= end => {
                panic!("connect: {error}\n{}", harness::diagnostics(server));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

async fn reflect(channel: Channel, message: MessageRequest) -> MessageResponse {
    bounded("reflection", async {
        let request = ServerReflectionRequest {
            host: String::new(),
            message_request: Some(message),
        };
        let mut client = ServerReflectionClient::new(channel);
        let mut stream = client
            .server_reflection_info(tokio_stream::iter([request]))
            .await
            .expect("reflection response")
            .into_inner();
        stream
            .message()
            .await
            .expect("reflection message")
            .expect("one reflection response")
            .message_response
            .expect("reflection result")
    })
    .await
}

async fn bounded<T>(name: &str, operation: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), operation)
        .await
        .unwrap_or_else(|_| panic!("{name} did not complete within 10 seconds"))
}

fn transcode(descriptors: &Path, operation: &str, message: &str, data: &[u8]) -> Vec<u8> {
    let mut child = Command::new("protoc")
        .arg(format!("--descriptor_set_in={}", descriptors.display()))
        .arg(format!("--{operation}={message}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start protoc");
    child
        .stdin
        .take()
        .expect("protoc stdin")
        .write_all(data)
        .expect("write protoc input");
    let output = child.wait_with_output().expect("wait for protoc");
    assert!(output.status.success(), "{:?}", output);
    output.stdout
}

async fn unary(
    channel: Channel,
    path: &str,
    request: Request<Bytes>,
) -> Result<
    (
        tonic::metadata::MetadataMap,
        Bytes,
        tonic::metadata::MetadataMap,
    ),
    Status,
> {
    bounded("unary call", async {
        let response = call(channel, path, request).await?;
        let headers = response.metadata().clone();
        let mut stream = response.into_inner();
        let message = stream.message().await?.expect("one unary response message");
        assert!(stream.message().await?.is_none(), "extra response message");
        let trailers = stream.trailers().await?.expect("response trailers");
        Ok((headers, message, trailers))
    })
    .await
}

async fn call(
    channel: Channel,
    path: &str,
    request: Request<Bytes>,
) -> Result<tonic::Response<tonic::Streaming<Bytes>>, Status> {
    bounded("response headers", async {
        let mut client = tonic::client::Grpc::new(channel);
        client.ready().await.expect("client ready");
        client
            .server_streaming(request, path.parse().expect("method path"), RawCodec)
            .await
    })
    .await
}

#[tokio::test]
async fn identity_responses_preserve_wire_bytes_and_metadata() {
    struct Case {
        name: &'static str,
        message: Bytes,
        prefix: &'static [u8; 5],
    }
    let mut limit_message = vec![b'x'; 4 * 1024 * 1024];
    limit_message[..5].copy_from_slice(b"\x0a\xfb\xff\xff\x01");
    let cases = [
        Case {
            name: "empty_payload",
            message: Bytes::new(),
            prefix: b"\0\0\0\0\0",
        },
        Case {
            name: "binary_payload",
            message: Bytes::from_static(b"\x0a\x03\0\xc3\xbf"),
            prefix: b"\0\0\0\0\x05",
        },
        Case {
            name: "at_default_response_limit",
            message: Bytes::from(limit_message),
            prefix: b"\0\0\x40\0\0",
        },
    ];
    let mut server = spawn("", None);
    let _channel = connect(&mut server).await;
    for case in cases {
        bounded(case.name, async {
            let tcp = tokio::net::TcpStream::connect(server.addr).await.unwrap();
            let (mut client, connection) = h2::client::handshake(tcp).await.unwrap();
            let request = http::Request::builder()
                .method("POST")
                .uri(format!("http://{}/e2e.v1.Echo/Echo", server.addr))
                .header("content-type", "application/grpc")
                .header("te", "trailers")
                .header("grpc-accept-encoding", "gzip")
                .header("x-repeat", "first")
                .header("x-repeat", "second")
                .header("x-data-bin", "AP8=")
                .header("x-data-bin", "dHdv")
                .body(())
                .unwrap();
            let (response, mut upload) = client.send_request(request, false).unwrap();
            upload
                .send_data(Bytes::copy_from_slice(case.prefix), false)
                .unwrap();
            upload.send_data(case.message.clone(), true).unwrap();
            let receive = async {
                let response = response.await.unwrap();
                assert_eq!(response.status(), 200, "{}", case.name);
                let headers = response.headers();
                assert_eq!(headers["content-type"], "application/grpc", "{}", case.name);
                assert!(!headers.contains_key("grpc-encoding"), "{}", case.name);
                assert!(!headers.contains_key("grpc-status"), "{}", case.name);
                assert_eq!(
                    headers
                        .get_all("x-repeat")
                        .iter()
                        .map(|value| value.to_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["first", "second"],
                    "{}",
                    case.name
                );
                let mut body = response.into_body();
                let mut wire = Vec::new();
                while let Some(data) = body.data().await {
                    let data = data.unwrap();
                    wire.extend_from_slice(&data);
                    body.flow_control().release_capacity(data.len()).unwrap();
                }
                assert_eq!(wire.len(), 5 + case.message.len(), "{}", case.name);
                assert_eq!(&wire[..5], case.prefix, "{}", case.name);
                assert_eq!(&wire[5..], case.message.as_ref(), "{}", case.name);
                let trailers = body.trailers().await.unwrap().expect("status trailers");
                assert_eq!(trailers["grpc-status"], "0", "{}", case.name);
                assert_eq!(trailers["x-result"], "completed", "{}", case.name);
                assert_eq!(
                    trailers
                        .get_all("x-data-bin")
                        .iter()
                        .map(|value| value.to_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["AP8", "dHdv"],
                    "{}",
                    case.name
                );
            };
            tokio::select! {
                () = receive => {},
                result = connection => panic!("{}: connection closed: {result:?}", case.name),
            }
        })
        .await;
    }
}

#[tokio::test]
async fn response_gzip_requires_configuration_and_client_acceptance() {
    struct Case {
        name: &'static str,
        compression: &'static str,
        request_gzip: bool,
        accept_gzip: bool,
        response_encoding: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "default_with_gzip_client",
            compression: "",
            request_gzip: false,
            accept_gzip: true,
            response_encoding: None,
        },
        Case {
            name: "enabled_with_gzip_client",
            compression: "[grpc.compression.gzip]\nenabled = true\n",
            request_gzip: false,
            accept_gzip: true,
            response_encoding: Some("gzip"),
        },
        Case {
            name: "disabled_with_gzip_client",
            compression: "[grpc.compression.gzip]\nenabled = false\n",
            request_gzip: false,
            accept_gzip: true,
            response_encoding: None,
        },
        Case {
            name: "enabled_with_identity_client",
            compression: "[grpc.compression.gzip]\nenabled = true\n",
            request_gzip: false,
            accept_gzip: false,
            response_encoding: None,
        },
        Case {
            name: "gzip_request_with_response_gzip_disabled",
            compression: "[grpc.compression.gzip]\nenabled = false\n",
            request_gzip: true,
            accept_gzip: true,
            response_encoding: None,
        },
        Case {
            name: "gzip_request_with_identity_client",
            compression: "[grpc.compression.gzip]\nenabled = false\n",
            request_gzip: true,
            accept_gzip: false,
            response_encoding: None,
        },
    ];
    for case in cases {
        let mut server = spawn(case.compression, None);
        let channel = connect(&mut server).await;
        bounded(case.name, async {
            let mut client = tonic::client::Grpc::new(channel);
            if case.request_gzip {
                client = client.send_compressed(tonic::codec::CompressionEncoding::Gzip);
            }
            if case.accept_gzip {
                client = client.accept_compressed(tonic::codec::CompressionEncoding::Gzip);
            }
            client.ready().await.expect("client ready");
            let message = Bytes::from_static(b"\x0a\x03\0\xc3\xbf");
            let response = client
                .server_streaming(
                    Request::new(message.clone()),
                    "/e2e.v1.Echo/Echo".parse().unwrap(),
                    RawCodec,
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("{}: {error}\n{}", case.name, harness::diagnostics(&server))
                });
            assert_eq!(
                response
                    .metadata()
                    .get("grpc-encoding")
                    .map(|value| value.to_str().unwrap()),
                case.response_encoding,
                "{}",
                case.name
            );
            let mut stream = response.into_inner();
            assert_eq!(
                stream.message().await.unwrap(),
                Some(message),
                "{}",
                case.name
            );
            assert!(stream.message().await.unwrap().is_none(), "{}", case.name);
            let trailers = stream.trailers().await.unwrap().expect("response trailers");
            assert_eq!(
                trailers.get("x-result").unwrap(),
                "completed",
                "{}",
                case.name
            );
        })
        .await;
    }
}

#[tokio::test]
async fn reflection_calls_php_without_server_protoc_or_client_schemas() {
    let mut server = spawn("", None);
    let channel = connect(&mut server).await;
    let MessageResponse::ListServicesResponse(services) =
        reflect(channel.clone(), MessageRequest::ListServices(String::new())).await
    else {
        panic!("list services response required");
    };
    let names: Vec<_> = services.service.into_iter().map(|s| s.name).collect();
    assert!(names.iter().any(|name| name == "e2e.v1.Echo"), "{names:?}");
    assert!(
        names
            .iter()
            .any(|name| name == "grpc.reflection.v1.ServerReflection")
    );
    assert!(
        names
            .iter()
            .any(|name| name == "grpc.reflection.v1alpha.ServerReflection")
    );
    let service_name = names
        .iter()
        .find(|name| !name.starts_with("grpc.reflection."))
        .expect("application service");

    let MessageResponse::FileDescriptorResponse(response) = reflect(
        channel.clone(),
        MessageRequest::FileContainingSymbol(service_name.clone()),
    )
    .await
    else {
        panic!("descriptor response required");
    };
    let mut descriptors = FileDescriptorSet {
        file: response
            .file_descriptor_proto
            .iter()
            .map(|bytes| {
                prost_types::FileDescriptorProto::decode(bytes.as_slice()).expect("descriptor")
            })
            .collect(),
    };
    let dependency = descriptors
        .file
        .iter()
        .flat_map(|file| &file.dependency)
        .next()
        .expect("timestamp import")
        .clone();
    let MessageResponse::FileDescriptorResponse(response) =
        reflect(channel.clone(), MessageRequest::FileByFilename(dependency)).await
    else {
        panic!("imported descriptor response required");
    };
    descriptors.file.extend(
        response
            .file_descriptor_proto
            .iter()
            .map(|bytes| prost_types::FileDescriptorProto::decode(bytes.as_slice()).unwrap()),
    );
    assert!(
        descriptors
            .file
            .iter()
            .any(|file| file.name() == "google/protobuf/timestamp.proto")
    );
    let service = descriptors
        .file
        .iter()
        .find_map(|file| {
            file.service
                .iter()
                .find(|service| format!("{}.{}", file.package(), service.name()) == *service_name)
        })
        .expect("discovered Echo service");
    let method = service.method.first().expect("discovered method");
    let input = method.input_type().trim_start_matches('.');
    let output = method.output_type().trim_start_matches('.');
    let path = server.dir.join("discovered.pb");
    std::fs::write(&path, descriptors.encode_to_vec()).expect("write discovered schemas");
    let text = b"text: \"reflection to PHP\"\nsent_at {\n  seconds: 123\n  nanos: 456\n}\n";
    let message = transcode(&path, "encode", input, text);
    let mut request = Request::new(Bytes::from(message));
    request
        .metadata_mut()
        .append("x-repeat", "first".parse().unwrap());
    request
        .metadata_mut()
        .append("x-repeat", "second".parse().unwrap());
    request.metadata_mut().append_bin(
        "x-data-bin",
        tonic::metadata::MetadataValue::from_bytes(b"\0\xff"),
    );
    request.metadata_mut().append_bin(
        "x-data-bin",
        tonic::metadata::MetadataValue::from_bytes(b"two"),
    );
    let route = format!("/{service_name}/{}", method.name());
    let (headers, response, trailers) =
        unary(channel, &route, request).await.expect("PHP response");
    assert_eq!(headers.get("x-protocol").unwrap(), "grpc");
    let repeated: Vec<_> = headers
        .get_all("x-repeat")
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(repeated, ["first", "second"]);
    assert_eq!(trailers.get("x-result").unwrap(), "completed");
    let binary: Vec<_> = trailers
        .get_all_bin("x-data-bin")
        .iter()
        .map(|value| value.to_bytes().unwrap())
        .collect();
    assert_eq!(
        binary,
        [Bytes::from_static(b"\0\xff"), Bytes::from_static(b"two")]
    );
    assert_eq!(transcode(&path, "decode", output, &response), text);
    let pid = headers.get("x-worker").unwrap().to_str().unwrap();
    let boot: serde_json::Value = serde_json::from_slice(
        &std::fs::read(server.dir.join(format!("boot-{pid}.json"))).expect("boot services"),
    )
    .expect("boot JSON");
    assert_eq!(
        boot,
        serde_json::json!([[
            "e2e.v1.Echo",
            [
                ["Echo", "e2e.v1.Message", "e2e.v1.Message", "unary"],
                ["Fail", "e2e.v1.Message", "e2e.v1.Message", "unary"],
                ["Hold", "e2e.v1.Message", "e2e.v1.Message", "unary"],
            ]
        ]])
    );
}

#[tokio::test]
async fn php_decodes_messages_with_nested_imports_and_google_types() {
    struct Case {
        name: &'static str,
        request: &'static [u8],
        reply: &'static [u8],
    }
    let cases = [
        Case {
            name: "shipping_with_nested_messages_and_google_types",
            request: include_bytes!("fixtures/grpc/complex/shipping-request.textproto"),
            reply: include_bytes!("fixtures/grpc/complex/shipping-reply.textproto"),
        },
        Case {
            name: "pickup_with_new_values_in_the_same_worker",
            request: include_bytes!("fixtures/grpc/complex/pickup-request.textproto"),
            reply: include_bytes!("fixtures/grpc/complex/pickup-reply.textproto"),
        },
    ];
    let dir = harness::scratch_dir();
    let generated = dir.join("generated");
    std::fs::create_dir(&generated).expect("create generated PHP directory");
    std::fs::copy(
        harness::fixture_path("grpc/complex/dispatcher.php"),
        dir.join("grpc.php"),
    )
    .expect("copy complex dispatcher");
    let addr = free_addr();
    let protos = harness::fixture_path("grpc/complex/proto");
    let config = format!(
        "[grpc]\nlisten = '{addr}'\nprotos = ['{}']\n[grpc.pool]\nentrypoint = 'grpc.php'\nmode = 'dispatcher'\nprocesses = 1\n[supervisor]\nprocess_control_timeout_secs = 5\n",
        protos.display(),
    );
    let mut server = harness::spawn_staged_config(dir, addr, &config, &[("PATH", "")]);
    let channel = connect(&mut server).await;
    let mut descriptors = FileDescriptorSet::default();
    let mut pending = vec![MessageRequest::FileContainingSymbol(
        "e2e.orders.v1.Orders".into(),
    )];
    while let Some(query) = pending.pop() {
        let MessageResponse::FileDescriptorResponse(response) =
            reflect(channel.clone(), query).await
        else {
            panic!("descriptor response required");
        };
        for bytes in response.file_descriptor_proto {
            let file = prost_types::FileDescriptorProto::decode(bytes.as_slice())
                .expect("imported descriptor");
            if descriptors.file.iter().any(|known| known.name == file.name) {
                continue;
            }
            pending.extend(
                file.dependency
                    .iter()
                    .cloned()
                    .map(MessageRequest::FileByFilename),
            );
            descriptors.file.push(file);
        }
    }
    let files: BTreeSet<_> = descriptors.file.iter().map(|file| file.name()).collect();
    assert_eq!(
        files,
        BTreeSet::from([
            "api/orders.proto",
            "model/order.proto",
            "common/customer.proto",
            "google/protobuf/any.proto",
            "google/protobuf/struct.proto",
            "google/protobuf/timestamp.proto",
        ]),
        "reflection must include the full import graph",
    );
    let path = server.dir.join("discovered.pb");
    std::fs::write(&path, descriptors.encode_to_vec()).expect("write discovered schemas");
    let output = Command::new("protoc")
        .arg(format!("--descriptor_set_in={}", path.display()))
        .arg(format!("--php_out={}", generated.display()))
        .args([
            "api/orders.proto",
            "model/order.proto",
            "common/customer.proto",
        ])
        .output()
        .expect("generate PHP protobuf classes");
    assert!(output.status.success(), "{output:?}");

    let mut worker = None;
    for case in cases {
        let message = transcode(
            &path,
            "encode",
            "e2e.orders.v1.InspectRequest",
            case.request,
        );
        let expected = transcode(&path, "encode", "e2e.orders.v1.InspectReply", case.reply);
        let (headers, response, _) = unary(
            channel.clone(),
            "/e2e.orders.v1.Orders/Inspect",
            Request::new(Bytes::from(message)),
        )
        .await
        .unwrap_or_else(|error| {
            panic!("{}: {error}\n{}", case.name, harness::diagnostics(&server))
        });
        let response = String::from_utf8(transcode(
            &path,
            "decode",
            "e2e.orders.v1.InspectReply",
            &response,
        ))
        .expect("response text format");
        let expected = String::from_utf8(transcode(
            &path,
            "decode",
            "e2e.orders.v1.InspectReply",
            &expected,
        ))
        .expect("expected text format");
        assert_eq!(
            response, expected,
            "{}: PHP must select and serialize the request fields",
            case.name,
        );
        let pid = headers.get("x-worker").expect("PHP worker header").clone();
        assert_eq!(worker.get_or_insert(pid.clone()), &pid, "{}", case.name);
    }
}

#[derive(PartialEq, prost::Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<prost_types::Any>,
}

#[tokio::test]
async fn php_failure_preserves_initial_metadata_and_rich_status_trailers() {
    let mut server = spawn("", None);
    let channel = connect(&mut server).await;
    let response = call(
        channel,
        "/e2e.v1.Echo/Fail",
        Request::new(Bytes::from_static(b"\x0a\x04oops")),
    )
    .await
    .expect("separate initial response metadata");
    assert!(response.metadata().get("x-worker").is_some());
    assert!(response.metadata().get("grpc-status").is_none());
    let mut stream = response.into_inner();
    let error = bounded("failure trailers", stream.message())
        .await
        .expect_err("failed RPC");
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    assert_eq!(error.message(), "invalid % value");
    assert_eq!(error.metadata().get("x-result").unwrap(), "completed");
    let status = RpcStatus::decode(error.details()).expect("google.rpc.Status");
    assert_eq!(status.code, 3);
    assert_eq!(status.message, "invalid % value");
    assert_eq!(
        status.details,
        [prost_types::Any {
            type_url: "type.googleapis.com/e2e.v1.Message".into(),
            value: b"\x0a\x04oops".to_vec(),
        }]
    );
}

async fn wait_file(server: &Server, name: &str, value: &str) {
    let end = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(server.dir.join(name))
            .ok()
            .as_deref()
            == Some(value)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "waiting for {name}={value}\n{}",
            harness::diagnostics(server)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn deadline_call(addr: SocketAddr, method: &str, id: &str, timeout: &str) -> u8 {
    bounded("deadline call", async {
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .expect("deadline connection");
    let (mut client, connection) = h2::client::handshake(tcp).await.expect("HTTP/2 handshake");
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/{method}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("x-id", id)
        .header("grpc-timeout", timeout)
        .body(())
        .unwrap();
    let (response, mut body) = client
        .send_request(request, false)
        .expect("send deadline request");
    body.send_data(Bytes::from_static(&[0; 5]), true)
        .expect("empty protobuf message");
    let receive = async {
        let response = response.await.expect("deadline response");
        assert_eq!(response.status(), 200);
        let initial_status = response.headers().get("grpc-status").cloned();
        let mut body = response.into_body();
        while let Some(data) = body.data().await {
            let data = data.expect("response data");
            body.flow_control().release_capacity(data.len()).unwrap();
        }
        let trailers = body.trailers().await.expect("deadline trailers");
        let status = initial_status
            .or_else(|| {
                trailers
                    .as_ref()
                    .and_then(|fields| fields.get("grpc-status").cloned())
            })
            .expect("gRPC status");
        status.to_str().unwrap().parse().unwrap()
    };
    tokio::select! {
        status = receive => status,
        result = connection => panic!("deadline connection closed: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(5)) => panic!("host deadline did not complete"),
    }
    })
    .await
}

#[tokio::test]
async fn queued_deadlines_discard_work_while_reflection_stays_available() {
    let mut server = spawn("", None);
    let channel = connect(&mut server).await;
    let mut request = Request::new(Bytes::new());
    request
        .metadata_mut()
        .insert("x-id", "holder".parse().unwrap());
    let active = unary(channel.clone(), "/e2e.v1.Echo/Hold", request);
    let probes = async {
        wait_file(&server, "active", "holder").await;
        let reflection = tokio::time::timeout(
            Duration::from_secs(1),
            reflect(channel.clone(), MessageRequest::ListServices(String::new())),
        )
        .await
        .expect("reflection must not wait for PHP");
        assert!(matches!(
            reflection,
            MessageResponse::ListServicesResponse(_)
        ));
        let status = deadline_call(server.addr, "e2e.v1.Echo/Echo", "expired", "20m").await;
        assert_eq!(status, 4, "queued deadline");
        std::fs::write(server.dir.join("release"), "ready").expect("release holder");
    };
    let (result, ()) = tokio::join!(active, probes);
    result.expect("holder response");
    unary(channel, "/e2e.v1.Echo/Echo", Request::new(Bytes::new()))
        .await
        .expect("next call");
    let calls = std::fs::read_to_string(server.dir.join("calls")).expect("PHP delivery log");
    assert!(
        !calls.contains(":expired"),
        "expired call reached PHP: {calls}"
    );
}

#[tokio::test]
async fn active_deadlines_cancel_php_and_allow_the_next_call() {
    let mut server = spawn("", None);
    let channel = connect(&mut server).await;
    unary(
        channel.clone(),
        "/e2e.v1.Echo/Echo",
        Request::new(Bytes::new()),
    )
    .await
    .expect("worker ready");
    let call = deadline_call(server.addr, "e2e.v1.Echo/Hold", "active", "2S");
    let (status, ()) = tokio::join!(call, wait_file(&server, "active", "active"));
    assert_eq!(status, 4, "active deadline");
    wait_file(&server, "cancelled", "active").await;
    unary(channel, "/e2e.v1.Echo/Echo", Request::new(Bytes::new()))
        .await
        .expect("next call");
}

#[tokio::test]
async fn client_reset_cancels_php_and_the_worker_serves_the_next_call() {
    let mut server = spawn("", None);
    let channel = connect(&mut server).await;
    let mut request = Request::new(Bytes::new());
    request
        .metadata_mut()
        .insert("x-id", "reset".parse().unwrap());
    {
        let pending = unary(channel.clone(), "/e2e.v1.Echo/Hold", request);
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("hold completed before client reset: {result:?}"),
            () = wait_file(&server, "active", "reset") => {},
        }
    }
    wait_file(&server, "cancelled", "reset").await;
    let (_, message, _) = unary(channel, "/e2e.v1.Echo/Echo", Request::new(Bytes::new()))
        .await
        .expect("next call");
    assert!(message.is_empty());
}

#[tokio::test]
async fn combined_pools_reload_with_the_prepared_registry() {
    let http = free_addr();
    let mut server = spawn("", Some(http));
    let channel = connect(&mut server).await;
    let (headers, _, _) = unary(channel, "/e2e.v1.Echo/Echo", Request::new(Bytes::new()))
        .await
        .expect("first call");
    let first_pid = headers
        .get("x-worker")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let (status, body) =
        harness::http_get(http, "/", Duration::from_secs(5)).expect("HTTP response");
    assert_eq!((status, body.as_slice()), (200, b"http-ok".as_slice()));

    std::fs::write(
        server.dir.join("proto/echo.proto"),
        "invalid schema after preparation",
    )
    .expect("change source");
    harness::signal(server.pid(), libc::SIGUSR2);
    let end = tokio::time::Instant::now() + BOOT;
    loop {
        let channel = connect(&mut server).await;
        if let Ok((headers, _, _)) =
            unary(channel, "/e2e.v1.Echo/Echo", Request::new(Bytes::new())).await
            && headers.get("x-worker").unwrap().to_str().unwrap() != first_pid
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "gRPC worker did not reload\n{}",
            harness::diagnostics(&server)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (status, body) =
        harness::http_get(http, "/", Duration::from_secs(5)).expect("HTTP after reload");
    assert_eq!((status, body.as_slice()), (200, b"http-ok".as_slice()));
}

#[tokio::test]
async fn shutdown_cancels_php_and_reflection_before_process_escalation() {
    let mut server = spawn("", None);
    let channel = connect(&mut server).await;
    let mut request = Request::new(Bytes::new());
    request
        .metadata_mut()
        .insert("x-id", "shutdown".parse().unwrap());
    let active = unary(channel.clone(), "/e2e.v1.Echo/Hold", request);
    let shutdown = async {
        wait_file(&server, "active", "shutdown").await;
        let (sender, requests) = tokio::sync::mpsc::channel(1);
        sender
            .send(ServerReflectionRequest {
                host: String::new(),
                message_request: Some(MessageRequest::ListServices(String::new())),
            })
            .await
            .expect("reflection request");
        let mut client = ServerReflectionClient::new(channel);
        let mut response = bounded(
            "open reflection",
            client.server_reflection_info(tokio_stream::wrappers::ReceiverStream::new(requests)),
        )
        .await
        .expect("open reflection")
        .into_inner();
        assert!(
            bounded("reflection result", response.message())
                .await
                .expect("reflection result")
                .is_some()
        );
        harness::signal(server.pid(), libc::SIGQUIT);
        let end = tokio::time::Instant::now() + Duration::from_secs(4);
        let closing = tokio::time::timeout_at(end, response.message())
            .await
            .expect("reflection closes before five-second process escalation");
        assert!(
            !matches!(closing, Ok(Some(_))),
            "unexpected reflection reply"
        );
        let status = loop {
            if let Some(status) = server.try_status() {
                break status;
            }
            assert!(
                tokio::time::Instant::now() < end,
                "worker exit exceeded drain grace\n{}",
                harness::diagnostics(&server)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(
            status.success(),
            "{status}\n{}",
            harness::diagnostics(&server)
        );
        assert_eq!(
            std::fs::read_to_string(server.dir.join("cancelled")).expect("PHP cancellation"),
            "shutdown"
        );
        drop(sender);
    };
    let (active, ()) = tokio::join!(active, shutdown);
    assert!(active.is_err(), "active PHP response was cancelled");
}

#[test]
fn invalid_application_schemas_fail_before_worker_startup() {
    struct Case {
        name: &'static str,
        schema: &'static str,
        diagnostic: &'static str,
    }
    let cases = [
        Case {
            name: "invalid_syntax",
            schema: "syntax = this is invalid;",
            diagnostic: "echo.proto",
        },
        Case {
            name: "missing_import",
            schema: "syntax = 'proto3'; import 'missing.proto';",
            diagnostic: "missing.proto",
        },
        Case {
            name: "application_streaming",
            schema: "syntax = 'proto3'; message M {} service S { rpc Stream(stream M) returns (M); }",
            diagnostic: "streaming",
        },
    ];
    for case in cases {
        let mut server = spawn_schema("", None, case.schema);
        let status = server.wait_exit(BOOT).expect("startup exit");
        assert!(!status.success(), "{}: {status}", case.name);
        let log = std::fs::read_to_string(server.dir.join("server.log")).expect("startup log");
        assert!(log.contains(case.diagnostic), "{}: {log}", case.name);
        assert!(
            !std::fs::read_dir(&server.dir).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("boot-")),
            "{}: PHP started",
            case.name
        );
    }
}
