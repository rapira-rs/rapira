use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use extension_api::{
    Addr, BoxError, BoxFuture, Handler, HttpRequest, HttpResponse, Middleware, Next, Peer, Php,
    Protocol, grpc,
};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use tonic::codec::CompressionEncoding;
use tonic::server::{Grpc, UnaryService};
use tonic::{Request, Response, Status};

use crate::codec::{EnvelopeBody, IdentityBody, RawCodec};
use crate::deadline::{self, Deadline, DeadlineBody};
use crate::reflection::Reflection;
use crate::{Config, metadata, response};

pub(crate) struct Shared {
    pub cfg: Config,
    php: Php,
    chain: Arc<[Arc<dyn Middleware>]>,
    reflection: Option<Reflection>,
    pub inflight: Arc<AtomicUsize>,
}

impl Shared {
    pub fn new(cfg: Config, php: Php) -> anyhow::Result<Self> {
        let reflection = cfg.reflection.then(|| Reflection::new(&cfg)).transpose()?;
        let chain = cfg.interceptors.clone().into();
        Ok(Self {
            cfg,
            php,
            chain,
            reflection,
            inflight: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[derive(Clone, Copy)]
struct RequestState {
    deadline: Option<Deadline>,
}

struct Inflight(Arc<AtomicUsize>);
impl Drop for Inflight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ResponseBody {
    inner: extension_api::Body,
    _guard: Inflight,
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

pub(crate) struct RapiraService {
    shared: Arc<Shared>,
    remote: Addr,
    server: Addr,
}

impl RapiraService {
    pub fn new(shared: Arc<Shared>, remote: Addr, server: Addr) -> Self {
        Self {
            shared,
            remote,
            server,
        }
    }
}

impl hyper::service::Service<http::Request<hyper::body::Incoming>> for RapiraService {
    type Response = HttpResponse;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<HttpResponse, Infallible>>;

    fn call(&self, req: http::Request<hyper::body::Incoming>) -> Self::Future {
        let req = req.map(|body| body.map_err(BoxError::from).boxed_unsync());
        let future = handle(
            Arc::clone(&self.shared),
            req,
            self.remote.clone(),
            self.server.clone(),
        );
        Box::pin(async move { Ok(future.await) })
    }
}

pub(crate) fn handle(
    shared: Arc<Shared>,
    req: HttpRequest,
    remote: Addr,
    server: Addr,
) -> impl Future<Output = HttpResponse> + Send + 'static {
    let received = Instant::now();
    let received_at = std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let deadline = deadline::parse(req.headers(), received, received_at);
    shared.inflight.fetch_add(1, Ordering::AcqRel);
    let guard = Inflight(Arc::clone(&shared.inflight));
    async move {
        let peer = Peer {
            remote,
            server,
            https: false,
            received_at,
        };
        let result = match deadline {
            Ok(deadline) => dispatch(shared, req, peer, deadline).await,
            Err(status) => Err(status),
        };
        result.unwrap_or_else(response::error).map(|inner| {
            ResponseBody {
                inner,
                _guard: guard,
            }
            .boxed_unsync()
        })
    }
}

async fn dispatch(
    shared: Arc<Shared>,
    mut req: HttpRequest,
    peer: Peer,
    deadline: Option<Deadline>,
) -> Result<HttpResponse, Status> {
    if deadline.is_some_and(Deadline::expired) {
        return Err(deadline::status());
    }
    if req.method() != http::Method::POST {
        let mut res = HttpResponse::new(extension_api::empty_body());
        *res.status_mut() = http::StatusCode::METHOD_NOT_ALLOWED;
        res.headers_mut().insert("allow", "POST".parse().unwrap());
        return Ok(res);
    }
    if !matches!(
        req.headers()
            .get("content-type")
            .and_then(|value| value.as_bytes().split(|&byte| byte == b';').next())
            .map(<[u8]>::trim_ascii),
        Some(b"application/grpc" | b"application/grpc+proto")
    ) {
        let mut res = HttpResponse::new(extension_api::empty_body());
        *res.status_mut() = http::StatusCode::UNSUPPORTED_MEDIA_TYPE;
        return Ok(res);
    }
    req.extensions_mut().insert(Protocol::Grpc);
    req.extensions_mut().insert(peer);
    req.extensions_mut().insert(RequestState { deadline });
    if let Some(deadline) = deadline {
        req = req.map(|body| DeadlineBody::new(body, deadline, false).boxed_unsync());
    }
    let next = Next::new(Arc::clone(&shared.chain), shared);
    let mut response = if let Some(deadline) = deadline {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline.expires_at.into()) => return Err(deadline::status()),
            response = next.run(req) => response::rejection(response),
        }
    } else {
        response::rejection(next.run(req).await)
    };
    if deadline.is_some_and(Deadline::expired) {
        return Err(deadline::status());
    }
    if let Some(deadline) = deadline
        && !response.headers().contains_key("grpc-status")
    {
        response = response.map(|body| DeadlineBody::new(body, deadline, true).boxed_unsync());
    }
    Ok(response)
}

impl Handler for Shared {
    fn call(&self, req: HttpRequest) -> BoxFuture<'_, HttpResponse> {
        Box::pin(self.route(req))
    }
}

impl Shared {
    async fn route(&self, req: HttpRequest) -> HttpResponse {
        if let Some(service) = self
            .reflection
            .as_ref()
            .and_then(|reflection| reflection.route(req.uri().path()))
        {
            return Reflection::call(service, req, self.cfg.max_request_message_size).await;
        }
        if !self.cfg.registry.has_method(req.uri().path()) {
            return response::error(Status::unimplemented("Unknown method"));
        }
        let Some(peer) = req.extensions().get::<Peer>().cloned() else {
            return response::error(Status::internal("Request context missing"));
        };
        let Some(state) = req.extensions().get::<RequestState>().copied() else {
            return response::error(Status::internal("Request context missing"));
        };
        let metadata = match metadata::decode(req.headers()) {
            Ok(metadata) => metadata,
            Err(status) => return response::error(status),
        };
        let reply_metadata = OnceLock::new();
        let reply_message = OnceLock::new();
        let service = PhpUnary {
            php: &self.php,
            request: Some(grpc::Request {
                method: req.uri().path()[1..].to_owned(),
                message: Bytes::new(),
                metadata,
                remote: peer.remote,
                tls: None,
                received_at: peer.received_at,
                deadline: state.deadline.map(|deadline| deadline.unix),
                expires_at: state.deadline.map(|deadline| deadline.expires_at),
            }),
            reply_metadata: &reply_metadata,
            reply_message: &reply_message,
            max_response_message_size: self.cfg.max_response_message_size,
        };
        let mut grpc = Grpc::new(RawCodec)
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(self.cfg.max_request_message_size)
            .max_encoding_message_size(self.cfg.max_response_message_size);
        if self.cfg.gzip_responses {
            grpc = grpc.send_compressed(CompressionEncoding::Gzip);
        }
        let mut response = grpc.unary(service, req.map(EnvelopeBody::unary)).await;
        if !response.headers().contains_key("grpc-encoding")
            && let Some(message) = reply_message.into_inner()
        {
            response = match IdentityBody::new(message) {
                Ok(body) => response.map(|_| tonic::body::Body::new(body)),
                Err(status) => status.into_http(),
            };
        }
        match reply_metadata.into_inner() {
            Some(metadata) => response::attach(response, metadata),
            None => response::boxed(response),
        }
    }
}

struct PhpUnary<'a> {
    php: &'a Php,
    request: Option<grpc::Request>,
    reply_metadata: &'a OnceLock<response::Metadata>,
    reply_message: &'a OnceLock<Bytes>,
    max_response_message_size: usize,
}

impl<'a> UnaryService<Bytes> for PhpUnary<'a> {
    type Response = Bytes;
    type Future = BoxFuture<'a, Result<Response<Bytes>, Status>>;

    fn call(&mut self, message: Request<Bytes>) -> Self::Future {
        let mut request = self.request.take().unwrap();
        request.message = message.into_inner();
        let php = self.php;
        let metadata = self.reply_metadata;
        let reply_message = self.reply_message;
        let limit = self.max_response_message_size;
        Box::pin(async move {
            if request
                .expires_at
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                return Err(deadline::status());
            }
            let reply = php.exec_grpc(request).await.map_err(response::host)?;
            let _ = metadata.set(response::Metadata {
                headers: metadata::encode(reply.headers)?,
                trailers: metadata::encode(reply.trailers)?,
            });
            let message = reply.result.map_err(response::application)?;
            if message.len() > limit {
                return Err(Status::out_of_range("Response message exceeds size limit"));
            }
            let _ = reply_message.set(message.clone());
            Ok(Response::new(message))
        })
    }
}
