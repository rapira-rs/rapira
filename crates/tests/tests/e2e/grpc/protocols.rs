use super::*;

use http::{HeaderMap, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;

async fn post(
    addr: SocketAddr,
    path: &str,
    content_type: &str,
    headers: &[(&str, &str)],
    body: Bytes,
) -> (StatusCode, HeaderMap, Bytes) {
    bounded("HTTP/1.1 RPC", async {
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut client, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        let mut request = http::Request::builder()
            .method("POST")
            .uri(path)
            .header("host", addr.to_string())
            .header("content-type", content_type)
            .header("connect-protocol-version", "1");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let receive = async {
            let response = client
                .send_request(request.body(Full::new(body)).unwrap())
                .await
                .unwrap();
            let (parts, body) = response.into_parts();
            (
                parts.status,
                parts.headers,
                body.collect().await.unwrap().to_bytes(),
            )
        };
        tokio::select! {
            result = receive => result,
            result = connection => panic!("HTTP/1.1 connection ended: {result:?}"),
        }
    })
    .await
}

fn web_trailers(body: &[u8]) -> HeaderMap {
    assert_eq!(body[0], 0x80);
    let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    assert_eq!(body.len(), 5 + length);
    let mut trailers = HeaderMap::new();
    for line in std::str::from_utf8(&body[5..]).unwrap().lines() {
        let (name, value) = line.split_once(':').unwrap();
        trailers.append(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.trim().parse().unwrap(),
        );
    }
    trailers
}

#[tokio::test]
async fn http1_protocols_deliver_php_messages_metadata_and_errors() {
    struct Case {
        name: &'static str,
        content_type: &'static str,
        connect: bool,
        failure: bool,
        http: u16,
    }
    let cases = [
        Case {
            name: "grpc_web_success",
            content_type: "application/grpc-web+proto",
            connect: false,
            failure: false,
            http: 200,
        },
        Case {
            name: "grpc_web_failure",
            content_type: "application/grpc-web+proto",
            connect: false,
            failure: true,
            http: 200,
        },
        Case {
            name: "connect_success",
            content_type: "application/proto",
            connect: true,
            failure: false,
            http: 200,
        },
        Case {
            name: "connect_failure",
            content_type: "application/proto",
            connect: true,
            failure: true,
            http: 400,
        },
    ];
    let mut server = spawn("", None);
    let _channel = connect(&mut server).await;
    for case in cases {
        let method = if case.failure { "Fail" } else { "Echo" };
        let payload: &[u8] = if case.connect {
            b"\x0a\x04oops"
        } else {
            b"\0\0\0\0\x06\x0a\x04oops"
        };
        let (status, headers, body) = post(
            server.addr,
            &format!("/e2e.v1.Echo/{method}"),
            case.content_type,
            &[
                ("x-repeat", "first"),
                ("x-repeat", "second"),
                ("x-data-bin", "AP8="),
            ],
            Bytes::from_static(payload),
        )
        .await;
        assert_eq!(status, case.http, "{}", case.name);
        assert!(headers.contains_key("x-worker"), "{}", case.name);
        assert_eq!(headers["x-protocol"], "grpc", "{}", case.name);
        assert_eq!(
            headers
                .get_all("x-repeat")
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["first", "second"],
            "{}",
            case.name
        );
        if case.connect {
            assert_eq!(headers["trailer-x-result"], "completed", "{}", case.name);
            assert_eq!(headers["trailer-x-data-bin"], "AP8", "{}", case.name);
            if case.failure {
                let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    error,
                    serde_json::json!({
                        "code": "invalid_argument",
                        "message": "invalid % value",
                        "details": [{"type": "e2e.v1.Message", "value": "CgRvb3Bz"}],
                    }),
                    "{}",
                    case.name
                );
            } else {
                assert_eq!(body.as_ref(), b"\x0a\x04oops", "{}", case.name);
            }
        } else {
            let trailers = if case.failure {
                web_trailers(&body)
            } else {
                assert_eq!(&body[..11], b"\0\0\0\0\x06\x0a\x04oops", "{}", case.name);
                web_trailers(&body[11..])
            };
            assert_eq!(
                trailers["grpc-status"],
                if case.failure { "3" } else { "0" },
                "{}",
                case.name
            );
            assert_eq!(trailers["x-result"], "completed", "{}", case.name);
            assert_eq!(trailers["x-data-bin"], "AP8", "{}", case.name);
            if case.failure {
                assert_eq!(
                    trailers["grpc-message"], "invalid %25 value",
                    "{}",
                    case.name
                );
            }
        }
    }
}

#[tokio::test]
async fn http1_gzip_respects_configuration_and_decodes_requests() {
    use std::io::Read;

    struct Case {
        name: &'static str,
        connect: bool,
        enabled: bool,
        accept: bool,
        compressed: bool,
    }
    let cases = [
        Case {
            name: "connect_enabled",
            connect: true,
            enabled: true,
            accept: true,
            compressed: true,
        },
        Case {
            name: "connect_disabled",
            connect: true,
            enabled: false,
            accept: true,
            compressed: false,
        },
        Case {
            name: "connect_infers_request_encoding",
            connect: true,
            enabled: true,
            accept: false,
            compressed: true,
        },
        Case {
            name: "grpc_web_enabled",
            connect: false,
            enabled: true,
            accept: true,
            compressed: true,
        },
        Case {
            name: "grpc_web_disabled",
            connect: false,
            enabled: false,
            accept: true,
            compressed: false,
        },
        Case {
            name: "grpc_web_identity_client",
            connect: false,
            enabled: true,
            accept: false,
            compressed: false,
        },
    ];
    for case in cases {
        let mut server = spawn(
            &format!("[grpc.compression.gzip]\nenabled = {}\n", case.enabled),
            None,
        );
        let _channel = connect(&mut server).await;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"\x0a\x04gzip").unwrap();
        let payload = encoder.finish().unwrap();
        let mut wire = Vec::new();
        if !case.connect {
            wire.push(1);
            wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        }
        wire.extend_from_slice(&payload);
        let (content_type, encoding, accept) = if case.connect {
            ("application/proto", "content-encoding", "accept-encoding")
        } else {
            (
                "application/grpc-web+proto",
                "grpc-encoding",
                "grpc-accept-encoding",
            )
        };
        let mut request_headers = vec![(encoding, "gzip")];
        if case.accept {
            request_headers.push((accept, "gzip"));
        }
        let (status, headers, body) = post(
            server.addr,
            "/e2e.v1.Echo/Echo",
            content_type,
            &request_headers,
            Bytes::from(wire),
        )
        .await;
        assert_eq!(status, 200, "{}", case.name);
        assert_eq!(
            headers.get(encoding).map(|v| v.to_str().unwrap()),
            case.compressed.then_some("gzip"),
            "{}",
            case.name
        );
        let payload = if case.connect {
            body.as_ref()
        } else {
            assert_eq!(body[0], u8::from(case.compressed), "{}", case.name);
            let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
            let trailers = web_trailers(&body[5 + length..]);
            assert_eq!(trailers["grpc-status"], "0", "{}", case.name);
            &body[5..5 + length]
        };
        let decoded = if case.compressed {
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(payload)
                .read_to_end(&mut decoded)
                .unwrap();
            decoded
        } else {
            payload.to_vec()
        };
        assert_eq!(decoded, b"\x0a\x04gzip", "{}", case.name);
    }
}

#[tokio::test]
async fn http1_deadlines_cancel_php_and_allow_the_next_request() {
    struct Case {
        name: &'static str,
        connect: bool,
        timeout_header: &'static str,
        timeout: &'static str,
    }
    let cases = [
        Case {
            name: "connect",
            connect: true,
            timeout_header: "connect-timeout-ms",
            timeout: "2000",
        },
        Case {
            name: "grpc_web",
            connect: false,
            timeout_header: "grpc-timeout",
            timeout: "2S",
        },
    ];
    for case in cases {
        let mut server = spawn("", None);
        let channel = connect(&mut server).await;
        unary(
            channel.clone(),
            "/e2e.v1.Echo/Echo",
            Request::new(Bytes::new()),
        )
        .await
        .unwrap();
        let (content_type, body) = if case.connect {
            ("application/proto", b"".as_slice())
        } else {
            ("application/grpc-web+proto", b"\0\0\0\0\0".as_slice())
        };
        let response = post(
            server.addr,
            "/e2e.v1.Echo/Hold",
            content_type,
            &[(case.timeout_header, case.timeout), ("x-id", case.name)],
            Bytes::from_static(body),
        )
        .await;
        if case.connect {
            assert_eq!(response.0, 504, "{}", case.name);
            let error: serde_json::Value = serde_json::from_slice(&response.2).unwrap();
            assert_eq!(error["code"], "deadline_exceeded", "{}", case.name);
        } else {
            assert_eq!(response.0, 200, "{}", case.name);
            assert_eq!(
                web_trailers(&response.2)["grpc-status"],
                "4",
                "{}",
                case.name
            );
        }
        wait_file(&server, "cancelled", case.name).await;
        unary(channel, "/e2e.v1.Echo/Echo", Request::new(Bytes::new()))
            .await
            .unwrap();
    }
}
