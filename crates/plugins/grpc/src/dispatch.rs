use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use bytes::Bytes;
use connectrpc::dispatcher::{RequestStream, StreamingResult, UnaryResult};
use connectrpc::{
    CodecFormat, ConnectError, Dispatcher, EncodedBody, EncodedResponse, ErrorCode, ErrorDetail,
    MethodDescriptor, Payload, Protocol, RequestContext,
};
use extension_api::{Addr, Php, Rejected, RpcProtocol, RpcStatus, UnaryCall};

use crate::schema::Schema;

/// Routes the unary methods of the configured services to PHP.
pub(crate) struct PhpDispatcher {
    pub(crate) schema: Arc<Schema>,
    pub(crate) php: Php,
}

impl Dispatcher for PhpDispatcher {
    fn lookup(&self, path: &str) -> Option<MethodDescriptor> {
        self.schema
            .method(path)
            .map(|m| MethodDescriptor::unary(m.idempotent))
    }

    fn call_unary(
        &self,
        path: &str,
        ctx: RequestContext,
        request: Payload,
        format: CodecFormat,
    ) -> UnaryResult {
        let schema = Arc::clone(&self.schema);
        let php = self.php.clone();
        let path = path.to_owned();
        Box::pin(async move { unary(&schema, &php, path, ctx, request, format).await })
    }

    fn call_server_streaming(
        &self,
        _path: &str,
        _ctx: RequestContext,
        _request: Bytes,
        _format: CodecFormat,
    ) -> StreamingResult {
        Box::pin(async { Err(streaming()) })
    }

    fn call_client_streaming(
        &self,
        _path: &str,
        _ctx: RequestContext,
        _requests: RequestStream,
        _format: CodecFormat,
    ) -> UnaryResult {
        Box::pin(async { Err(streaming()) })
    }

    fn call_bidi_streaming(
        &self,
        _path: &str,
        _ctx: RequestContext,
        _requests: RequestStream,
        _format: CodecFormat,
    ) -> StreamingResult {
        Box::pin(async { Err(streaming()) })
    }
}

/// `lookup` reports no streaming method, so connectrpc never calls a streaming handler.
fn streaming() -> ConnectError {
    ConnectError::unimplemented("rapira serves unary methods only")
}

async fn unary(
    schema: &Schema,
    php: &Php,
    path: String,
    ctx: RequestContext,
    request: Payload,
    format: CodecFormat,
) -> Result<EncodedResponse, ConnectError> {
    let Some(method) = schema.method(&path) else {
        return Err(ConnectError::unimplemented(format!(
            "method not found: {path}"
        )));
    };
    let body = request.encoded()?;
    let message = match format {
        CodecFormat::Proto => body,
        CodecFormat::Json => schema
            .json_to_proto(method, &body)
            .map_err(|e| ConnectError::invalid_argument(format!("{e:#}")))?
            .into(),
        _ => {
            return Err(ConnectError::unimplemented(format!(
                "codec {format:?} is not supported"
            )));
        }
    };
    let protocol = ctx.protocol();
    let call = UnaryCall {
        method: path.clone(),
        protocol: match protocol {
            Some(Protocol::Grpc) => RpcProtocol::Grpc,
            Some(Protocol::GrpcWeb) => RpcProtocol::GrpcWeb,
            _ => RpcProtocol::Connect,
        },
        metadata: ctx.headers().clone(),
        deadline: ctx
            .deadline()
            .map(|d| unix_deadline(d, Instant::now(), SystemTime::now())),
        remote: ctx
            .extensions()
            .get::<Addr>()
            .cloned()
            .unwrap_or(Addr::Unix(None)),
        message,
    };
    let reply = match php.unary(call).await {
        Ok(Some(reply)) => reply,
        Ok(None) => {
            tracing::warn!(target: "grpc", "{path}: the worker lost the call");
            return Err(ConnectError::internal("internal error"));
        }
        Err(e) => {
            let reason = e
                .downcast_ref::<Rejected>()
                .map_or_else(|| e.to_string(), |r| r.reason.clone());
            return Err(ConnectError::unavailable(reason));
        }
    };
    let message = match reply.outcome {
        Ok(message) => message,
        Err(status) => {
            return Err(status_error(status, protocol)
                .with_headers(reply.headers)
                .with_trailers(reply.trailers));
        }
    };
    let body = match format {
        CodecFormat::Json => match schema.proto_to_json(method, &message) {
            Ok(json) => Bytes::from(json),
            Err(e) => {
                tracing::warn!(target: "grpc", "{path}: the reply does not decode: {e:#}");
                return Err(ConnectError::internal("internal error"));
            }
        },
        _ => message,
    };
    let mut response = EncodedResponse::new(EncodedBody::from(body));
    response.headers = reply.headers;
    response.trailers = reply.trailers;
    Ok(response)
}

/// A Connect error detail carries the bare type name. gRPC and gRPC-Web keep the `google.protobuf.Any` type URL whole, because connectrpc adds `type.googleapis.com/` only to a name without a `/`.
fn status_error(status: RpcStatus, protocol: Option<Protocol>) -> ConnectError {
    let whole_url = matches!(protocol, Some(Protocol::Grpc | Protocol::GrpcWeb));
    let code = ErrorCode::from_grpc_code(status.code).unwrap_or(ErrorCode::Unknown);
    let mut err = ConnectError::new(code, status.message);
    err.details = status
        .details
        .into_iter()
        .map(|(url, value)| ErrorDetail {
            type_url: match url.rsplit_once('/') {
                Some((_, name)) if !whole_url => name.to_owned(),
                _ => url,
            },
            value: Some(STANDARD_NO_PAD.encode(value)),
            debug: None,
        })
        .collect();
    err
}

/// The wall-clock time of `deadline`, in Unix seconds.
fn unix_deadline(deadline: Instant, now: Instant, wall: SystemTime) -> f64 {
    let wall = wall.duration_since(UNIX_EPOCH).unwrap_or_default();
    (wall + deadline.saturating_duration_since(now)).as_secs_f64()
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::time::Duration;

    use extension_api::{Backend, UnaryReply};

    use super::*;
    use crate::testing::{self, Fields, fields};

    const ECHO_PATH: &str = "rapira.test.v1.EchoService/Echo";

    /// What the scripted PHP answers.
    #[derive(Clone, Copy)]
    enum Script {
        Reply(&'static [u8]),
        /// Code, message, one detail as (type URL, value).
        Fail(u32, &'static str, &'static str, &'static [u8]),
        Refuse,
        Lose,
    }

    enum Want {
        Body(&'static [u8]),
        /// Code, message (None: not checked), details as (type URL, base64 value).
        Error(
            ErrorCode,
            Option<&'static str>,
            &'static [(&'static str, &'static str)],
        ),
    }

    struct Scripted {
        answer: Mutex<Option<anyhow::Result<Option<UnaryReply>>>>,
        seen: Mutex<Option<UnaryCall>>,
    }

    impl Backend for Scripted {
        fn exec(
            &self,
            _req: extension_api::Request,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<extension_api::Reply>> + Send + '_>>
        {
            unreachable!("the gRPC plugin sends no HTTP request")
        }

        fn unary(
            &self,
            call: UnaryCall,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<UnaryReply>>> + Send + '_>> {
            *self.seen.lock().unwrap() = Some(call);
            let answer = self.answer.lock().unwrap().take().expect("one call");
            Box::pin(async move { answer })
        }
    }

    struct Case {
        name: &'static str,
        protocol: Option<Protocol>,
        format: CodecFormat,
        request: &'static [u8],
        /// The connection extension that names the peer.
        peer: Option<Addr>,
        script: Script,
        /// The response halves that PHP sets.
        headers: Fields,
        trailers: Fields,
        /// The message that PHP gets. None: PHP never sees the call.
        sent: Option<&'static [u8]>,
        /// The protocol and the remote address that PHP sees.
        rpc: RpcProtocol,
        remote: Addr,
        want: Want,
    }

    fn inet(addr: &str) -> Addr {
        Addr::Inet(addr.parse::<SocketAddr>().unwrap())
    }

    /// Sources: the connectrpc error model (error.rs), D9 for the type URL, the Connect error-details spec, and RFC 4648: 0a 01 78 is `CgF4`.
    #[tokio::test]
    async fn call_unary_maps_between_connectrpc_and_php() {
        const ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";
        let cases = [
            Case {
                name: "proto passes through",
                protocol: None,
                format: CodecFormat::Proto,
                request: &[0x0a, 0x02, 0x68, 0x69],
                peer: None,
                script: Script::Reply(&[0x0a, 0x02, 0x6f, 0x6b]),
                headers: &[],
                trailers: &[],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Connect,
                remote: Addr::Unix(None),
                want: Want::Body(&[0x0a, 0x02, 0x6f, 0x6b]),
            },
            Case {
                name: "json both ways",
                protocol: Some(Protocol::Connect),
                format: CodecFormat::Json,
                request: br#"{"text":"hi"}"#,
                peer: None,
                script: Script::Reply(&[0x0a, 0x02, 0x6f, 0x6b]),
                headers: &[],
                trailers: &[],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Connect,
                remote: Addr::Unix(None),
                want: Want::Body(br#"{"text":"ok"}"#),
            },
            Case {
                name: "malformed json never reaches php",
                protocol: Some(Protocol::Connect),
                format: CodecFormat::Json,
                request: b"{",
                peer: None,
                script: Script::Reply(&[]),
                headers: &[],
                trailers: &[],
                sent: None,
                rpc: RpcProtocol::Connect,
                remote: Addr::Unix(None),
                want: Want::Error(ErrorCode::InvalidArgument, None, &[]),
            },
            Case {
                name: "undecodable reply under json",
                protocol: Some(Protocol::Connect),
                format: CodecFormat::Json,
                request: br#"{"text":"hi"}"#,
                peer: None,
                script: Script::Reply(&[0xff]),
                headers: &[],
                trailers: &[],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Connect,
                remote: Addr::Unix(None),
                want: Want::Error(ErrorCode::Internal, Some("internal error"), &[]),
            },
            Case {
                name: "status maps with details for connect",
                protocol: Some(Protocol::Connect),
                format: CodecFormat::Proto,
                request: &[0x0a, 0x02, 0x68, 0x69],
                peer: None,
                script: Script::Fail(5, "no invoice", ERROR_INFO, &[0x0a, 0x01, 0x78]),
                headers: &[],
                trailers: &[],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Connect,
                remote: Addr::Unix(None),
                want: Want::Error(
                    ErrorCode::NotFound,
                    Some("no invoice"),
                    &[("google.rpc.ErrorInfo", "CgF4")],
                ),
            },
            Case {
                name: "custom type url stays whole on grpc",
                protocol: Some(Protocol::Grpc),
                format: CodecFormat::Proto,
                request: &[0x0a, 0x02, 0x68, 0x69],
                peer: None,
                script: Script::Fail(
                    5,
                    "no invoice",
                    "example.com/acme.Detail",
                    &[0x0a, 0x01, 0x78],
                ),
                headers: &[],
                trailers: &[],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Grpc,
                remote: Addr::Unix(None),
                want: Want::Error(
                    ErrorCode::NotFound,
                    Some("no invoice"),
                    &[("example.com/acme.Detail", "CgF4")],
                ),
            },
            Case {
                name: "halves ride a success",
                protocol: Some(Protocol::Grpc),
                format: CodecFormat::Proto,
                request: &[0x0a, 0x02, 0x68, 0x69],
                peer: None,
                script: Script::Reply(&[0x0a, 0x02, 0x6f, 0x6b]),
                headers: &[("x-h", "v")],
                trailers: &[("x-t", "w")],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Grpc,
                remote: Addr::Unix(None),
                want: Want::Body(&[0x0a, 0x02, 0x6f, 0x6b]),
            },
            Case {
                name: "halves ride an error",
                protocol: Some(Protocol::Grpc),
                format: CodecFormat::Proto,
                request: &[0x0a, 0x02, 0x68, 0x69],
                peer: None,
                script: Script::Fail(5, "no invoice", ERROR_INFO, &[0x0a, 0x01, 0x78]),
                headers: &[("x-h", "v")],
                trailers: &[("x-t", "w")],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Grpc,
                remote: Addr::Unix(None),
                want: Want::Error(
                    ErrorCode::NotFound,
                    Some("no invoice"),
                    &[(ERROR_INFO, "CgF4")],
                ),
            },
            Case {
                name: "refusal",
                protocol: Some(Protocol::Grpc),
                format: CodecFormat::Proto,
                request: &[0x0a, 0x02, 0x68, 0x69],
                peer: None,
                script: Script::Refuse,
                headers: &[],
                trailers: &[],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Grpc,
                remote: Addr::Unix(None),
                // PHP never saw the call; the client gets the reason without the HTTP status of Rejected
                want: Want::Error(ErrorCode::Unavailable, Some("worker pool saturated"), &[]),
            },
            Case {
                name: "lost",
                protocol: Some(Protocol::Grpc),
                format: CodecFormat::Proto,
                request: &[0x0a, 0x02, 0x68, 0x69],
                peer: None,
                script: Script::Lose,
                headers: &[],
                trailers: &[],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::Grpc,
                remote: Addr::Unix(None),
                want: Want::Error(ErrorCode::Internal, Some("internal error"), &[]),
            },
            Case {
                name: "request facts reach php",
                protocol: Some(Protocol::GrpcWeb),
                format: CodecFormat::Proto,
                request: &[0x0a, 0x02, 0x68, 0x69],
                peer: Some(inet("203.0.113.7:4000")),
                script: Script::Reply(&[0x0a, 0x02, 0x6f, 0x6b]),
                headers: &[],
                trailers: &[],
                sent: Some(&[0x0a, 0x02, 0x68, 0x69]),
                rpc: RpcProtocol::GrpcWeb,
                remote: inet("203.0.113.7:4000"),
                want: Want::Body(&[0x0a, 0x02, 0x6f, 0x6b]),
            },
        ];

        for case in cases {
            let answer = match case.script {
                Script::Reply(body) => Ok(Some(UnaryReply {
                    headers: fields(case.headers),
                    trailers: fields(case.trailers),
                    outcome: Ok(Bytes::from_static(body)),
                })),
                Script::Fail(code, message, url, value) => Ok(Some(UnaryReply {
                    headers: fields(case.headers),
                    trailers: fields(case.trailers),
                    outcome: Err(RpcStatus {
                        code,
                        message: message.into(),
                        details: vec![(url.into(), Bytes::from_static(value))],
                    }),
                })),
                Script::Refuse => Err(anyhow::Error::new(extension_api::Rejected {
                    status: 503,
                    reason: "worker pool saturated".into(),
                })),
                Script::Lose => Ok(None),
            };
            let backend = Arc::new(Scripted {
                answer: Mutex::new(Some(answer)),
                seen: Mutex::new(None),
            });
            let dispatcher = PhpDispatcher {
                schema: testing::schema(),
                php: Php::new(backend.clone()),
            };
            let mut extensions = http::Extensions::new();
            if let Some(peer) = case.peer.clone() {
                extensions.insert(peer);
            }
            let wall = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();
            let ctx = RequestContext::new(fields(&[("x-a", "1")]))
                .with_protocol(case.protocol)
                .with_deadline(Some(Instant::now() + Duration::from_secs(60)))
                .with_extensions(extensions);
            let payload = Payload::new(Bytes::from_static(case.request), case.format);
            let got = dispatcher
                .call_unary(ECHO_PATH, ctx, payload, case.format)
                .await;

            let seen = backend.seen.lock().unwrap().take();
            assert_eq!(
                seen.as_ref().map(|c| c.message.as_ref()),
                case.sent,
                "{}: the message PHP got",
                case.name
            );
            if let Some(call) = seen {
                assert_eq!(call.method, ECHO_PATH, "{}", case.name);
                assert_eq!(call.protocol, case.rpc, "{}", case.name);
                assert_eq!(call.remote, case.remote, "{}", case.name);
                assert_eq!(call.metadata, fields(&[("x-a", "1")]), "{}", case.name);
                assert!(
                    call.deadline.is_some_and(|d| d > wall + 50.0),
                    "{}: deadline {:?}",
                    case.name,
                    call.deadline
                );
            }

            match (case.want, got) {
                (Want::Body(body), Ok(resp)) => {
                    assert_eq!(resp.body.into_contiguous(), body, "{}", case.name);
                    assert_eq!(resp.headers, fields(case.headers), "{}", case.name);
                    assert_eq!(resp.trailers, fields(case.trailers), "{}", case.name);
                }
                (Want::Error(code, message, details), Err(err)) => {
                    assert_eq!(err.code, code, "{}", case.name);
                    if let Some(message) = message {
                        assert_eq!(err.message.as_deref(), Some(message), "{}", case.name);
                    }
                    let got: Vec<_> = err
                        .details
                        .iter()
                        .map(|d| (d.type_url.as_str(), d.value.as_deref(), d.debug.is_some()))
                        .collect();
                    let want: Vec<_> = details
                        .iter()
                        .map(|&(url, value)| (url, Some(value), false))
                        .collect();
                    assert_eq!(got, want, "{}", case.name);
                    assert_eq!(
                        err.response_headers(),
                        &fields(case.headers),
                        "{}",
                        case.name
                    );
                    assert_eq!(err.trailers(), &fields(case.trailers), "{}", case.name);
                }
                (_, Ok(resp)) => panic!("{}: expected an error, got {resp:?}", case.name),
                (_, Err(err)) => panic!("{}: expected a body, got {err:?}", case.name),
            }
        }
    }

    #[test]
    fn unix_deadline_converts_monotonic_to_wall() {
        struct Case {
            name: &'static str,
            deadline: Duration,
            now: Duration,
            expected: f64,
        }
        let cases = [
            Case {
                name: "1.5 s ahead",
                deadline: Duration::from_millis(1500),
                now: Duration::ZERO,
                expected: 101.5,
            },
            Case {
                name: "already passed",
                deadline: Duration::ZERO,
                now: Duration::from_secs(1),
                expected: 100.0,
            },
        ];
        let base = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(100);
        for case in cases {
            let got = unix_deadline(base + case.deadline, base + case.now, wall);
            assert!((got - case.expected).abs() < 1e-9, "{}: {got}", case.name);
        }
    }
}
