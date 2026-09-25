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
    mut ctx: RequestContext,
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
        metadata: std::mem::take(ctx.headers_mut()),
        deadline: ctx
            .deadline()
            .map(|d| unix_deadline(d, Instant::now(), SystemTime::now())),
        remote: ctx
            .extensions_mut()
            .remove::<Addr>()
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
    use std::time::Duration;

    use super::*;

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
