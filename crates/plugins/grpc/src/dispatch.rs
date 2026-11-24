//! The `PhpDispatcher`: one call through the intake to PHP, the JSON transcoding, and the PHP reply, stream or status as a connectrpc response or error.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use bytes::Bytes;
use connectrpc::dispatcher::{RequestStream, StreamingResult, UnaryResult};
use connectrpc::{
    CodecFormat, ConnectError, Dispatcher, EncodedBody, EncodedResponse, EncodedStream,
    MethodDescriptor, MethodKind, Payload, Protocol, RequestContext, Response, http,
};
use futures_util::stream;
use rapira_sapi::Addr;
use rapira_sapi::work::{Intake, Unit, now_unix_f64};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout_at;

use crate::call::{Deadline, expired};
use crate::php::GrpcState;
use crate::php::call::{Reply, StreamReply};
use crate::schema::{Method, Schema};
use crate::serve::InFlight;
use crate::{Call, Part};

/// Routes the unary and server-streaming methods of the configured services to PHP.
pub(crate) struct PhpDispatcher {
    pub(crate) schema: Arc<Schema>,
    pub(crate) intake: Intake,
    /// The calls in flight that the drain counts (serve.rs).
    pub(crate) inflight: Arc<AtomicUsize>,
}

impl Dispatcher for PhpDispatcher {
    fn lookup(&self, path: &str) -> Option<MethodDescriptor> {
        self.schema
            .method(path)
            .map(|m| MethodDescriptor::from_kind(m.kind).with_idempotent(m.idempotent))
    }

    fn call_unary(
        &self,
        path: &str,
        ctx: RequestContext,
        request: Payload,
        format: CodecFormat,
    ) -> UnaryResult {
        let schema = Arc::clone(&self.schema);
        let intake = self.intake.clone();
        let path = path.to_owned();
        Box::pin(async move { unary(&schema, &intake, path, ctx, request, format).await })
    }

    fn call_server_streaming(
        &self,
        path: &str,
        ctx: RequestContext,
        request: Bytes,
        format: CodecFormat,
    ) -> StreamingResult {
        let schema = Arc::clone(&self.schema);
        let intake = self.intake.clone();
        let inflight = Arc::clone(&self.inflight);
        let path = path.to_owned();
        Box::pin(server_streaming(
            schema, intake, inflight, path, ctx, request, format,
        ))
    }

    fn call_client_streaming(
        &self,
        _path: &str,
        _ctx: RequestContext,
        _requests: RequestStream,
        _format: CodecFormat,
    ) -> UnaryResult {
        Box::pin(async { Err(request_stream()) })
    }

    fn call_bidi_streaming(
        &self,
        _path: &str,
        _ctx: RequestContext,
        _requests: RequestStream,
        _format: CodecFormat,
    ) -> StreamingResult {
        Box::pin(async { Err(request_stream()) })
    }
}

/// `lookup` reports no method with a request stream, so connectrpc never calls these handlers.
fn request_stream() -> ConnectError {
    ConnectError::unimplemented("rapira serves unary and server-streaming methods only")
}

/// Most gRPC clients accept gzip on every call. As in grpc-go, a gRPC reply uses gzip only when the request did. See https://github.com/grpc/grpc-go/blob/v1.84.0/server.go#L1408-L1421
fn plain_reply(ctx: &RequestContext) -> bool {
    matches!(ctx.protocol(), Some(Protocol::Grpc | Protocol::GrpcWeb))
        && ctx.header("grpc-encoding").is_none_or(|v| v == "identity")
}

/// Builds the call of one request: its message as binary protobuf, and the request facts that PHP reads. It takes the headers out of `ctx`.
fn call(
    schema: &Schema,
    method: &Method,
    path: &str,
    ctx: &mut RequestContext,
    body: Bytes,
    format: CodecFormat,
) -> Result<Call, ConnectError> {
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
    Ok(Call {
        method: path.to_owned(),
        kind: method.kind,
        protocol: ctx.protocol().unwrap_or(Protocol::Connect),
        metadata: std::mem::take(ctx.headers_mut()),
        deadline: ctx.deadline().map(|at| Deadline {
            at,
            unix: now_unix_f64() + at.saturating_duration_since(Instant::now()).as_secs_f64(),
        }),
        remote: ctx
            .extensions_mut()
            .remove::<Addr>()
            .expect("serve puts an Addr on every connection"),
        message,
    })
}

/// Encodes one output message in the format of the client.
fn encode(
    schema: &Schema,
    path: &str,
    format: CodecFormat,
    message: Bytes,
) -> Result<Bytes, ConnectError> {
    if format != CodecFormat::Json {
        return Ok(message);
    }
    let method = schema.method(path).expect("lookup found the route");
    match schema.proto_to_json(method, &message) {
        Ok(json) => Ok(Bytes::from(json)),
        Err(e) => {
            tracing::warn!(target: "grpc", "{path}: the reply does not decode: {e:#}");
            Err(ConnectError::internal("internal error"))
        }
    }
}

/// A sender can close because PHP saw the deadline before the IO timer ran.
fn missing_reply(path: &str, until: Option<Instant>) -> ConnectError {
    if expired(until) {
        return deadline();
    }
    tracing::warn!(target: "grpc", "{path}: the app lost the call");
    ConnectError::internal("internal error")
}

async fn unary(
    schema: &Schema,
    intake: &Intake,
    path: String,
    mut ctx: RequestContext,
    request: Payload,
    format: CodecFormat,
) -> Result<EncodedResponse, ConnectError> {
    let method = schema.method(&path).expect("lookup found the route");
    // connectrpc gives a Connect unary request to this handler for a method of each kind.
    if method.kind != MethodKind::Unary {
        return Err(ConnectError::unimplemented(format!(
            "{path} is a streaming method"
        )));
    }
    let plain_reply = plain_reply(&ctx);
    let call = call(schema, method, &path, &mut ctx, request.encoded()?, format)?;
    drop(request);
    let protocol = call.protocol;
    let bytes = call.message.len();
    let until = call.until();
    let (tx, reply) = oneshot::channel();
    let call = GrpcState::new(call, Reply::Unary(tx));
    // The intake refuses the call before dispatch, so PHP never sees it.
    if let Err(e) = intake.submit(Unit::Dispatcher(Box::new(call)), bytes).await {
        return Err(ConnectError::unavailable(e.to_string()));
    }
    // A closed channel means that PHP lost the call. The drop of this future closes the call for PHP.
    let Ok(reply) = reply.await else {
        return Err(missing_reply(&path, until));
    };
    let message = match reply.outcome {
        Ok(message) => message,
        Err(err) => {
            return Err(err
                .with_headers(reply.headers)
                .with_trailers(reply.trailers));
        }
    };
    let body = encode(schema, &path, format, message)?;
    let mut response = EncodedResponse::new(EncodedBody::from(body));
    response.headers = reply.headers;
    response.trailers = reply.trailers;
    if plain_reply {
        response.compress = Some(false);
    }
    // A Connect reply is compressed when the client accepts it, so a cache keys a Connect GET reply on Accept-Encoding. See https://www.rfc-editor.org/rfc/rfc9110#section-12.5.5
    if protocol == Protocol::Connect {
        response.headers.append(
            http::header::VARY,
            http::HeaderValue::from_static("accept-encoding"),
        );
    }
    Ok(response)
}

async fn server_streaming(
    schema: Arc<Schema>,
    intake: Intake,
    inflight: Arc<AtomicUsize>,
    path: String,
    mut ctx: RequestContext,
    request: Bytes,
    format: CodecFormat,
) -> Result<Response<EncodedStream>, ConnectError> {
    let method = schema.method(&path).expect("lookup found the route");
    let plain_reply = plain_reply(&ctx);
    let call = call(&schema, method, &path, &mut ctx, request, format)?;
    let until = call.until();
    let bytes = call.message.len();
    let (head_tx, head) = oneshot::channel();
    // The channel holds one message. When the buffers of the connection are full, hyper stops the poll of the body, so the generator waits at its yield.
    let (parts_tx, parts) = mpsc::channel(1);
    let reply = StreamReply::new(head_tx, parts_tx);
    let call = GrpcState::new(call, Reply::Stream(reply));
    // The intake refuses the call before dispatch, so PHP never sees it.
    if let Err(e) = intake.submit(Unit::Dispatcher(Box::new(call)), bytes).await {
        return Err(ConnectError::unavailable(e.to_string()));
    }
    // This future holds both receivers until the head arrives, so its drop closes the call for PHP.
    let head = match head.await {
        Ok(Ok(head)) => head,
        // fail() before the first yield
        Ok(Err(err)) => return Err(err),
        Err(_) => return Err(missing_reply(&path, until)),
    };
    let body = Body {
        parts,
        until,
        schema,
        path,
        format,
        _counted: InFlight::start(&inflight),
    };
    let mut response = Response::stream(stream::try_unfold(body, Body::next));
    response.headers = head.headers;
    response.trailers = head.trailers;
    if plain_reply {
        response.compress = Some(false);
    }
    Ok(response)
}

/// The body of a streaming response after its head. It holds the parts that PHP sends. Its drop closes the call for PHP.
struct Body {
    parts: mpsc::Receiver<Part>,
    /// The deadline of the call. connectrpc bounds only the handler with it, so the body ends the stream there.
    until: Option<Instant>,
    schema: Arc<Schema>,
    path: String,
    format: CodecFormat,
    /// The handler returns at the head, so the body keeps the call in the count of the drain until the stream ends.
    _counted: InFlight,
}

impl Body {
    /// Returns one message of the response stream, with the body for the next step. Returns None when PHP ended the stream with success. An error ends the stream and drops the body.
    async fn next(mut self) -> Result<Option<(EncodedBody, Self)>, ConnectError> {
        let part = match self.until {
            None => self.parts.recv().await,
            // timeout_at polls recv first, so this check ends the stream at the deadline even when the generator always has a message ready.
            Some(until) if Instant::now() >= until => return Err(deadline()),
            Some(until) => match timeout_at(until.into(), self.parts.recv()).await {
                Ok(part) => part,
                Err(_) => return Err(deadline()),
            },
        };
        match part {
            Some(Part::Message(message)) => {
                let message = encode(&self.schema, &self.path, self.format, message)?;
                Ok(Some((EncodedBody::from(message), self)))
            }
            Some(Part::End) => Ok(None),
            Some(Part::Fail(err)) => Err(err),
            None => Err(missing_reply(&self.path, self.until)),
        }
    }
}

fn deadline() -> ConnectError {
    ConnectError::deadline_exceeded("request deadline exceeded")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_sapi::work::Held;
    use std::time::Duration;

    #[test]
    fn the_unary_pull_observes_the_deadline_with_its_reply_receiver_open() {
        struct Case {
            name: &'static str,
            until: Option<Instant>,
            cancelled: bool,
        }
        let now = Instant::now();
        let cases = [
            Case {
                name: "no deadline",
                until: None,
                cancelled: false,
            },
            Case {
                name: "live deadline",
                until: Some(now + Duration::from_secs(60)),
                cancelled: false,
            },
            Case {
                name: "expired deadline",
                until: Some(now - Duration::from_millis(1)),
                cancelled: true,
            },
        ];
        for case in cases {
            let req = Call {
                method: "rapira.test.v1.EchoService/Echo".to_owned(),
                kind: MethodKind::Unary,
                protocol: Protocol::Grpc,
                metadata: http::HeaderMap::new(),
                deadline: case.until.map(|at| Deadline { at, unix: 0.0 }),
                remote: Addr::Unix(None),
                message: Bytes::new(),
            };
            let (tx, _receiver) = oneshot::channel();
            let state = GrpcState::new(req, Reply::Unary(tx));
            assert_eq!(state.cancelled(), case.cancelled, "{}", case.name);
            assert_eq!(state.is_finalized(), case.cancelled, "{}", case.name);
        }
    }
}
