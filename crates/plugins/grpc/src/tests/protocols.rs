use super::*;

fn connect_request(path: &str, message: &'static [u8]) -> HttpRequest {
    let mut request = request(path, message);
    *request.version_mut() = http::Version::HTTP_11;
    request
        .headers_mut()
        .insert("content-type", "application/proto".parse().unwrap());
    request
        .headers_mut()
        .insert("connect-protocol-version", "1".parse().unwrap());
    request
}

struct OwnedBytes {
    bytes: Vec<u8>,
    _dropped: Dropped,
}

impl AsRef<[u8]> for OwnedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

fn tracked_bytes(bytes: Vec<u8>) -> (Bytes, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    (
        Bytes::from_owner(OwnedBytes {
            bytes,
            _dropped: Dropped(Arc::clone(&dropped)),
        }),
        dropped,
    )
}

#[tokio::test]
async fn removed_request_metadata_is_released_before_php_wait() {
    struct Remove;

    impl Middleware for Remove {
        fn handle<'a>(&'a self, mut req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
            req.headers_mut().remove("x-buffer");
            Box::pin(next.run(req))
        }
    }

    struct Case {
        name: &'static str,
        content_type: &'static str,
        connect: bool,
        deadline: bool,
    }
    let cases = [
        Case {
            name: "grpc",
            content_type: "application/grpc+proto",
            connect: false,
            deadline: false,
        },
        Case {
            name: "grpc_deadline",
            content_type: "application/grpc+proto",
            connect: false,
            deadline: true,
        },
        Case {
            name: "grpc_web",
            content_type: "application/grpc-web+proto",
            connect: false,
            deadline: false,
        },
        Case {
            name: "grpc_web_deadline",
            content_type: "application/grpc-web+proto",
            connect: false,
            deadline: true,
        },
        Case {
            name: "connect",
            content_type: "application/proto",
            connect: true,
            deadline: false,
        },
        Case {
            name: "connect_deadline",
            content_type: "application/proto",
            connect: true,
            deadline: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend {
            hold: true,
            ..Default::default()
        });
        let mut cfg = config();
        cfg.interceptors.push(Arc::new(Remove));
        let mut req = if case.connect {
            connect_request("/example.Echo/Call", b"\x0a\x02hi")
        } else {
            request("/example.Echo/Call", b"\0\0\0\0\x04\x0a\x02hi".as_slice())
        };
        req.headers_mut()
            .insert("content-type", case.content_type.parse().unwrap());
        if case.deadline {
            let (header, value) = if case.connect {
                ("connect-timeout-ms", "30000")
            } else {
                ("grpc-timeout", "30S")
            };
            req.headers_mut().insert(header, value.parse().unwrap());
        }
        let (bytes, dropped) = tracked_bytes(vec![b'a'; 4096]);
        req.headers_mut().insert(
            "x-buffer",
            http::HeaderValue::from_maybe_shared(bytes).unwrap(),
        );
        let mut pending = Box::pin(call(shared(cfg, &backend), req));
        assert!(
            poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_pending())).await,
            "{}",
            case.name
        );
        assert_eq!(backend.seen.lock().unwrap().len(), 1, "{}", case.name);
        assert!(dropped.load(Ordering::Acquire), "{}", case.name);
        backend.release.notify_one();
        let response = pending.await;
        assert_eq!(response.status(), http::StatusCode::OK, "{}", case.name);
        response.into_body().collect().await.unwrap();
    }
}

#[tokio::test]
async fn fragmented_unary_requests_release_consumed_buffers() {
    struct Case {
        name: &'static str,
        content_type: &'static str,
        framed: bool,
    }
    let cases = [
        Case {
            name: "grpc",
            content_type: "application/grpc+proto",
            framed: true,
        },
        Case {
            name: "grpc_web",
            content_type: "application/grpc-web+proto",
            framed: true,
        },
        Case {
            name: "connect",
            content_type: "application/proto",
            framed: false,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let payload = vec![0xa5; 65536];
        let wire = if case.framed {
            frame(&payload)
        } else {
            payload.clone()
        };
        let (first, first_dropped) = tracked_bytes(wire[..16384].to_vec());
        let (second, second_dropped) = tracked_bytes(wire[16384..32768].to_vec());
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        sender
            .send(Ok::<_, BoxError>(Frame::data(first)))
            .await
            .unwrap();
        sender.send(Ok(Frame::data(second))).await.unwrap();
        let mut req = request("/example.Echo/Call", b"".as_slice()).map(|_| {
            StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receiver)).boxed_unsync()
        });
        req.headers_mut()
            .insert("content-type", case.content_type.parse().unwrap());
        if !case.framed {
            req.headers_mut()
                .insert("connect-protocol-version", "1".parse().unwrap());
        }
        let mut pending = Box::pin(call(shared(config(), &backend), req));
        assert!(
            poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_pending())).await,
            "{}",
            case.name
        );
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
        assert!(
            first_dropped.load(Ordering::Acquire),
            "{}: first",
            case.name
        );
        assert!(
            second_dropped.load(Ordering::Acquire),
            "{}: second",
            case.name
        );
        sender
            .send(Ok(Frame::data(Bytes::copy_from_slice(&wire[32768..]))))
            .await
            .unwrap();
        drop(sender);
        let response = pending.await;
        assert_eq!(response.status(), http::StatusCode::OK, "{}", case.name);
        response.into_body().collect().await.unwrap();
        let seen = backend.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{}", case.name);
        assert_eq!(seen[0].message.as_ref(), payload, "{}", case.name);
    }
}

#[tokio::test]
async fn native_grpc_requires_http2() {
    struct Case {
        name: &'static str,
        version: http::Version,
        http: u16,
        dispatched: bool,
    }
    let cases = [
        Case {
            name: "http1",
            version: http::Version::HTTP_11,
            http: 505,
            dispatched: false,
        },
        Case {
            name: "http2",
            version: http::Version::HTTP_2,
            http: 200,
            dispatched: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut req = request("/example.Echo/Call", b"\0\0\0\0\0".as_slice());
        *req.version_mut() = case.version;
        let res = call(shared(config(), &backend), req).await;
        assert_eq!(res.status(), case.http, "{}", case.name);
        assert_eq!(
            !backend.seen.lock().unwrap().is_empty(),
            case.dispatched,
            "{}",
            case.name
        );
    }
}

#[tokio::test]
async fn connect_deadline_covers_interceptors_and_php_waits() {
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
        let mut req = connect_request("/example.Echo/Call", b"");
        req.headers_mut()
            .insert("connect-timeout-ms", "10".parse().unwrap());
        let res = call(shared(cfg, &backend), req).await;
        assert_eq!(res.status(), 504, "{}", case.name);
        let json: serde_json::Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(json["code"], "deadline_exceeded", "{}", case.name);
        assert_eq!(
            !backend.seen.lock().unwrap().is_empty(),
            case.dispatched,
            "{}",
            case.name
        );
        assert_eq!(
            backend.dropped.load(Ordering::Acquire),
            case.dispatched,
            "{}",
            case.name
        );
    }
}

#[tokio::test]
async fn connect_rejects_invalid_timeout_and_codec_before_php() {
    struct Case {
        name: &'static str,
        content_type: &'static str,
        timeout: &'static [&'static str],
        http: u16,
    }
    let cases = [
        Case {
            name: "negative_timeout",
            content_type: "application/proto",
            timeout: &["-1"],
            http: 400,
        },
        Case {
            name: "timeout_with_sign",
            content_type: "application/proto",
            timeout: &["+1"],
            http: 400,
        },
        Case {
            name: "timeout_with_unit",
            content_type: "application/proto",
            timeout: &["1m"],
            http: 400,
        },
        Case {
            name: "eleven_digit_timeout",
            content_type: "application/proto",
            timeout: &["10000000000"],
            http: 400,
        },
        Case {
            name: "duplicate_timeout",
            content_type: "application/proto",
            timeout: &["10", "20"],
            http: 400,
        },
        Case {
            name: "unsupported_json",
            content_type: "application/json",
            timeout: &[],
            http: 415,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut req = connect_request("/example.Echo/Call", b"");
        req.headers_mut()
            .insert("content-type", case.content_type.parse().unwrap());
        for timeout in case.timeout {
            req.headers_mut()
                .append("connect-timeout-ms", timeout.parse().unwrap());
        }
        let res = call(shared(config(), &backend), req).await;
        assert_eq!(res.status(), case.http, "{}", case.name);
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
    }
}

#[tokio::test]
async fn connect_errors_use_message_names_and_preserve_metadata() {
    struct Case {
        name: &'static str,
        type_url: &'static str,
    }
    let cases = [
        Case {
            name: "standard_type_url",
            type_url: "type.googleapis.com/example.Detail",
        },
        Case {
            name: "custom_type_url",
            type_url: "https://schemas.example.com/example.Detail",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        backend.replies.lock().unwrap().push_back(Ok(grpc::Reply {
            headers: vec![("x-header".into(), b"first".to_vec())],
            trailers: vec![("x-trailer".into(), b"last".to_vec())],
            result: Err(grpc::Status {
                code: 3,
                message: "invalid % value".into(),
                details: vec![grpc::ErrorDetail {
                    type_url: case.type_url.into(),
                    value: Bytes::from_static(b"\x08\x07"),
                }],
            }),
        }));
        let res = call(
            shared(config(), &backend),
            connect_request("/example.Echo/Call", b""),
        )
        .await;
        assert_eq!(res.status(), 400, "{}", case.name);
        assert_eq!(res.headers()["x-header"], "first", "{}", case.name);
        assert_eq!(res.headers()["trailer-x-trailer"], "last", "{}", case.name);
        let json: serde_json::Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "code": "invalid_argument",
                "message": "invalid % value",
                "details": [{"type": "example.Detail", "value": "CAc"}],
            }),
            "{}",
            case.name
        );
    }
}

#[tokio::test]
async fn unary_reflection_requests_stay_out_of_php() {
    struct Case {
        name: &'static str,
        path: &'static str,
    }
    let cases = [
        Case {
            name: "v1",
            path: "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
        },
        Case {
            name: "v1alpha",
            path: "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let res = call(
            shared(config(), &backend),
            connect_request(case.path, b"\x3a\0"),
        )
        .await;
        assert_eq!(res.status(), 501, "{}", case.name);
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
    }
}

#[tokio::test]
async fn unary_protocols_preserve_php_payload_and_metadata() {
    struct Case {
        name: &'static str,
        content_type: &'static str,
        framed: bool,
        connect: bool,
    }
    let cases = [
        Case {
            name: "native_grpc",
            content_type: "application/grpc+proto",
            framed: true,
            connect: false,
        },
        Case {
            name: "grpc_web_binary",
            content_type: "application/grpc-web+proto",
            framed: true,
            connect: false,
        },
        Case {
            name: "connect_binary",
            content_type: "application/proto",
            framed: false,
            connect: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        backend.replies.lock().unwrap().push_back(Ok(grpc::Reply {
            headers: vec![("x-worker".into(), b"php".to_vec())],
            trailers: vec![("x-result".into(), b"echo".to_vec())],
            result: Ok(Bytes::from_static(b"\x0a\x02hi")),
        }));
        let payload = if case.framed {
            b"\0\0\0\0\x04\x0a\x02hi".as_slice()
        } else {
            b"\x0a\x02hi".as_slice()
        };
        let wire = payload.to_vec();
        let mut req = request("/example.Echo/Call", wire);
        req.headers_mut()
            .insert("content-type", case.content_type.parse().unwrap());
        req.headers_mut()
            .insert("x-request-id", "demo".parse().unwrap());
        if case.connect {
            req.headers_mut()
                .insert("connect-protocol-version", "1".parse().unwrap());
        }
        let res = call(shared(config(), &backend), req).await;
        assert_eq!(res.status(), http::StatusCode::OK, "{}", case.name);
        let (headers, body, trailers) = collect(res).await;
        assert_eq!(headers["x-worker"], "php", "{}", case.name);
        let seen = backend.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{}", case.name);
        assert_eq!(seen[0].message.as_ref(), b"\x0a\x02hi", "{}", case.name);
        assert_eq!(seen[0].method, "example.Echo/Call", "{}", case.name);
        assert!(
            seen[0]
                .metadata
                .contains(&("x-request-id".into(), b"demo".to_vec())),
            "{}",
            case.name
        );
        if case.connect {
            assert_eq!(body.as_ref(), b"\x0a\x02hi", "{}", case.name);
            assert_eq!(headers["trailer-x-result"], "echo", "{}", case.name);
        } else if case.content_type.contains("grpc-web") {
            assert_eq!(&body[..9], b"\0\0\0\0\x04\x0a\x02hi", "{}", case.name);
            assert_eq!(body[9], 0x80, "{}", case.name);
            let length = u32::from_be_bytes(body[10..14].try_into().unwrap()) as usize;
            assert_eq!(body.len(), 14 + length, "{}", case.name);
            let fields = std::str::from_utf8(&body[14..]).unwrap();
            assert!(
                fields.lines().any(|line| line
                    .split_once(':')
                    .is_some_and(|(key, value)| key == "grpc-status" && value.trim() == "0")),
                "{}",
                case.name
            );
            assert!(
                fields.lines().any(|line| line
                    .split_once(':')
                    .is_some_and(|(key, value)| key == "x-result" && value.trim() == "echo")),
                "{}",
                case.name
            );
        } else {
            assert_eq!(body.as_ref(), b"\0\0\0\0\x04\x0a\x02hi", "{}", case.name);
            assert_eq!(
                status(&headers, &trailers).code(),
                tonic::Code::Ok,
                "{}",
                case.name
            );
            assert_eq!(trailers["x-result"], "echo", "{}", case.name);
        }
    }
}

#[tokio::test]
async fn reflection_is_available_on_each_binary_streaming_protocol() {
    struct Case {
        name: &'static str,
        content_type: &'static str,
        connect: bool,
    }
    let cases = [
        Case {
            name: "grpc_web",
            content_type: "application/grpc-web+proto",
            connect: false,
        },
        Case {
            name: "connect",
            content_type: "application/connect+proto",
            connect: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut req = request(
            "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
            b"\0\0\0\0\x02\x3a\0".as_slice(),
        );
        req.headers_mut()
            .insert("content-type", case.content_type.parse().unwrap());
        req.headers_mut()
            .insert("connect-protocol-version", "1".parse().unwrap());
        let (_, body, _) = collect(call(shared(config(), &backend), req).await).await;
        assert_eq!(body[0], 0, "{}", case.name);
        let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
        let response = pb::ServerReflectionResponse::decode(&body[5..5 + length]).unwrap();
        let Some(pb::server_reflection_response::MessageResponse::ListServicesResponse(services)) =
            response.message_response
        else {
            panic!("{}: list services response required", case.name);
        };
        let mut names: Vec<_> = services
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
        let end = &body[5 + length..];
        assert_eq!(end[0], if case.connect { 2 } else { 128 }, "{}", case.name);
        let length = u32::from_be_bytes(end[1..5].try_into().unwrap()) as usize;
        assert_eq!(end.len(), 5 + length, "{}", case.name);
        if case.connect {
            let status: serde_json::Value = serde_json::from_slice(&end[5..]).unwrap();
            assert!(status.get("error").is_none(), "{}", case.name);
        } else {
            assert!(
                std::str::from_utf8(&end[5..])
                    .unwrap()
                    .lines()
                    .any(|line| line
                        .split_once(':')
                        .is_some_and(|(key, value)| key == "grpc-status" && value.trim() == "0")),
                "{}",
                case.name
            );
        }
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
    }
}

#[tokio::test]
async fn reflection_rejects_response_only_frame_flags() {
    struct Case {
        name: &'static str,
        content_type: &'static str,
        wire: &'static [u8],
        connect: bool,
    }
    let cases = [
        Case {
            name: "grpc_web_trailer_flag",
            content_type: "application/grpc-web+proto",
            wire: b"\x80\0\0\0\x02{}",
            connect: false,
        },
        Case {
            name: "connect_end_stream_flag",
            content_type: "application/connect+proto",
            wire: b"\x02\0\0\0\x02{}",
            connect: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut req = request(
            "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
            case.wire,
        );
        req.headers_mut()
            .insert("content-type", case.content_type.parse().unwrap());
        let (_, body, _) = collect(call(shared(config(), &backend), req).await).await;
        if case.connect {
            assert_eq!(body[0], 2, "{}", case.name);
            let end: serde_json::Value = serde_json::from_slice(&body[5..]).unwrap();
            assert_eq!(end["error"]["code"], "internal", "{}", case.name);
        } else {
            assert_eq!(body[0], 128, "{}", case.name);
            assert!(
                std::str::from_utf8(&body[5..])
                    .unwrap()
                    .lines()
                    .any(|line| line
                        .split_once(':')
                        .is_some_and(|(key, value)| key == "grpc-status" && value.trim() == "13")),
                "{}",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn new_protocols_enforce_decompression_limits_and_encoding_support() {
    use std::io::Write;

    struct Case {
        name: &'static str,
        connect: bool,
        encoding: &'static str,
        http: u16,
        grpc: &'static str,
        code: &'static str,
    }
    let cases = [
        Case {
            name: "connect_decompression_limit",
            connect: true,
            encoding: "gzip",
            http: 429,
            grpc: "8",
            code: "resource_exhausted",
        },
        Case {
            name: "grpc_web_decompression_limit",
            connect: false,
            encoding: "gzip",
            http: 200,
            grpc: "8",
            code: "resource_exhausted",
        },
        Case {
            name: "connect_unsupported_encoding",
            connect: true,
            encoding: "zstd",
            http: 501,
            grpc: "12",
            code: "unimplemented",
        },
        Case {
            name: "grpc_web_unsupported_encoding",
            connect: false,
            encoding: "zstd",
            http: 200,
            grpc: "12",
            code: "unimplemented",
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&[42; 4097]).unwrap();
        let payload = encoder.finish().unwrap();
        let mut wire = Vec::new();
        if !case.connect {
            wire.push(1);
            wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        }
        wire.extend_from_slice(&payload);
        let mut req = request("/example.Echo/Call", wire);
        let (content_type, encoding) = if case.connect {
            ("application/proto", "content-encoding")
        } else {
            ("application/grpc-web+proto", "grpc-encoding")
        };
        req.headers_mut()
            .insert("content-type", content_type.parse().unwrap());
        req.headers_mut()
            .insert(encoding, case.encoding.parse().unwrap());
        let res = call(
            shared(
                Config {
                    max_request_message_size: 4096,
                    ..config()
                },
                &backend,
            ),
            req,
        )
        .await;
        assert_eq!(res.status(), case.http, "{}", case.name);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        if case.connect {
            let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(error["code"], case.code, "{}", case.name);
        } else {
            assert_eq!(body[0], 128, "{}", case.name);
            assert!(
                std::str::from_utf8(&body[5..])
                    .unwrap()
                    .lines()
                    .any(|line| line.split_once(':').is_some_and(|(key, value)| key
                        == "grpc-status"
                        && value.trim() == case.grpc)),
                "{}",
                case.name
            );
        }
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
    }
}
