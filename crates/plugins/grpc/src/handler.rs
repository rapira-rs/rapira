use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use connectrpc::compression::{CompressionPolicy, CompressionRegistry, GzipProvider};
use connectrpc::dispatcher::{MethodDescriptor, RequestStream, StreamingResult, UnaryResult};
use connectrpc::{
    CodecFormat, ConnectError, ConnectRpcService, DeadlinePolicy, Dispatcher, Limits, Payload,
    RequestContext, Router,
};
use extension_api::{
    Addr, BoxError, BoxFuture, Handler, HttpRequest, HttpResponse, Middleware, Next, Peer, Php,
    Protocol, grpc,
};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use tokio_stream::StreamExt;
use tower::ServiceExt;

use crate::deadline::{self, Deadline, DeadlineBody};
use crate::{Config, Registry, metadata, reflection, response};

pub(crate) struct Shared {
    pub cfg: Config,
    chain: Arc<[Arc<dyn Middleware>]>,
    service: ConnectRpcService<PhpDispatcher>,
    pub inflight: Arc<AtomicUsize>,
}

impl Shared {
    pub fn new(cfg: Config, php: Php) -> anyhow::Result<Self> {
        let limits = Limits::default()
            .with_max_request_body_size(cfg.max_request_message_size.saturating_add(5))
            .with_max_message_size(cfg.max_request_message_size);
        let dispatcher = PhpDispatcher {
            php,
            registry: Arc::clone(&cfg.registry),
            reflection: reflection::router(&cfg, limits)?,
            max_response_message_size: cfg.max_response_message_size,
        };
        let policy = if cfg.gzip_responses {
            CompressionPolicy::default().with_min_size(0)
        } else {
            CompressionPolicy::disabled()
        };
        let service = ConnectRpcService::new(dispatcher)
            .with_limits(limits)
            .with_compression(CompressionRegistry::new().register(GzipProvider::default()))
            .with_compression_policy(policy)
            .with_deadline_policy(DeadlinePolicy::new().with_enforce_on_streams(true));
        Ok(Self {
            chain: cfg.interceptors.clone().into(),
            cfg,
            service,
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
    _cancel: tokio::sync::oneshot::Sender<()>,
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
    mut req: HttpRequest,
    remote: Addr,
    server: Addr,
) -> impl Future<Output = HttpResponse> + Send + 'static {
    let received = Instant::now();
    let received_at = std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let headers = req.headers();
    let deadline = deadline::parse(headers, received, received_at);
    let framed =
        connectrpc::Protocol::detect(headers).is_some_and(|p| p.is_streaming && !p.is_text_mode);
    let errors = response::ErrorContext::new(headers);
    let unary = shared.cfg.registry.has_method(req.uri().path());
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    req = req.map(|body| {
        crate::request::RequestBody::new(
            body,
            cancelled,
            framed,
            unary,
            shared.cfg.max_request_message_size.saturating_add(5),
        )
        .boxed_unsync()
    });
    shared.inflight.fetch_add(1, Ordering::AcqRel);
    let guard = Inflight(Arc::clone(&shared.inflight));
    req.extensions_mut().insert(Protocol::Grpc);
    req.extensions_mut().insert(Peer {
        remote,
        server,
        https: false,
        received_at,
    });
    async move {
        let result = async {
            let deadline = deadline?;
            if deadline.is_some_and(Deadline::expired) {
                return Err(deadline::status());
            }
            req.extensions_mut().insert(RequestState { deadline });
            let next = Next::new(Arc::clone(&shared.chain), shared);
            let mut response = if let Some(deadline) = deadline {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline.expires_at.into()) => return Err(deadline::status()),
                    response = next.run(req) => response::rejection(response, &errors),
                }
            } else {
                response::rejection(next.run(req).await, &errors)
            };
            if let Some(deadline) = deadline {
                if deadline.expired() {
                    return Err(deadline::status());
                }
                response.headers_mut().remove(http::header::CONTENT_LENGTH);
                response = response.map(|body| DeadlineBody::new(body, deadline, errors.clone()).boxed_unsync());
            }
            Ok(response)
        }.await;
        result
            .unwrap_or_else(|error| errors.response(error))
            .map(|inner| {
                ResponseBody {
                    inner,
                    _guard: guard,
                    _cancel: cancel,
                }
                .boxed_unsync()
            })
    }
}

impl Handler for Shared {
    fn call(&self, req: HttpRequest) -> BoxFuture<'_, HttpResponse> {
        if req.version() != http::Version::HTTP_2
            && connectrpc::Protocol::detect(req.headers())
                .is_some_and(|p| p.protocol == connectrpc::Protocol::Grpc)
        {
            return Box::pin(async {
                http::Response::builder()
                    .status(http::StatusCode::HTTP_VERSION_NOT_SUPPORTED)
                    .body(extension_api::empty_body())
                    .unwrap()
            });
        }
        let mut service = self.service.clone();
        let gzip = self.cfg.gzip_responses
            && req
                .headers()
                .get_all("grpc-accept-encoding")
                .iter()
                .any(|value| {
                    value
                        .to_str()
                        .is_ok_and(|value| value.split(',').any(|item| item.trim() == "gzip"))
                });
        let grpc = connectrpc::Protocol::detect(req.headers()).is_some_and(|p| {
            matches!(
                p.protocol,
                connectrpc::Protocol::Grpc | connectrpc::Protocol::GrpcWeb
            )
        });
        if grpc && !gzip {
            service = service.with_compression_policy(CompressionPolicy::disabled());
        }
        Box::pin(async move {
            let req = req.map(|body| {
                body.map_err(|error: BoxError| std::io::Error::other(error))
                    .boxed_unsync()
            });
            let mut response = service
                .oneshot(req)
                .await
                .unwrap_or_else(|never| match never {});
            if grpc && !gzip {
                response.headers_mut().remove("grpc-encoding");
            }
            response::boxed(response)
        })
    }
}

struct PhpDispatcher {
    php: Php,
    registry: Arc<Registry>,
    reflection: Router,
    max_response_message_size: usize,
}

impl Dispatcher for PhpDispatcher {
    fn lookup(&self, path: &str) -> Option<MethodDescriptor> {
        if self.registry.has_method(&format!("/{path}")) {
            Some(MethodDescriptor::unary(false))
        } else {
            self.reflection.lookup(path)
        }
    }

    fn call_unary(
        &self,
        path: &str,
        ctx: RequestContext,
        request: Payload,
        format: CodecFormat,
    ) -> UnaryResult {
        if !self.registry.has_method(&format!("/{path}")) {
            return self.reflection.call_unary(path, ctx, request, format);
        }
        let path = path.to_owned();
        let php = self.php.clone();
        let limit = self.max_response_message_size;
        Box::pin(async move {
            if format != CodecFormat::Proto {
                return Err(ConnectError::unimplemented(
                    "PHP calls require binary protobuf messages",
                )
                .with_http_status(http::StatusCode::UNSUPPORTED_MEDIA_TYPE));
            }
            let peer = ctx
                .extensions()
                .get::<Peer>()
                .ok_or_else(|| ConnectError::internal("Request context missing"))?;
            let state = ctx
                .extensions()
                .get::<RequestState>()
                .ok_or_else(|| ConnectError::internal("Request context missing"))?;
            let expires_at = state.deadline.map(|d| d.expires_at);
            let request = grpc::Request {
                method: path,
                message: request.encoded()?,
                metadata: metadata::decode(ctx.headers())?,
                remote: peer.remote.clone(),
                tls: None,
                received_at: peer.received_at,
                deadline: state.deadline.map(|d| d.unix),
                expires_at,
            };
            let reply = php.exec_grpc(request).await.map_err(response::host)?;
            response::reply(
                reply,
                limit,
                ctx.protocol() == Some(connectrpc::Protocol::Connect),
            )
        })
    }

    fn call_server_streaming(
        &self,
        _: &str,
        _: RequestContext,
        _: Bytes,
        _: CodecFormat,
    ) -> StreamingResult {
        Box::pin(async { Err(ConnectError::unimplemented("PHP calls are unary")) })
    }

    fn call_client_streaming(
        &self,
        _: &str,
        _: RequestContext,
        _: RequestStream,
        _: CodecFormat,
    ) -> UnaryResult {
        Box::pin(async { Err(ConnectError::unimplemented("PHP calls are unary")) })
    }

    fn call_bidi_streaming(
        &self,
        path: &str,
        ctx: RequestContext,
        requests: RequestStream,
        format: CodecFormat,
    ) -> StreamingResult {
        let future = self
            .reflection
            .call_bidi_streaming(path, ctx, requests, format);
        let limit = self.max_response_message_size;
        Box::pin(async move {
            let mut response = future.await?;
            response.compress = Some(false);
            response.body = Box::pin(response.body.map(move |message| {
                let message = message?;
                if message.len() > limit {
                    return Err(ConnectError::resource_exhausted(
                        "Response message exceeds size limit",
                    ));
                }
                Ok(message)
            }));
            Ok(response)
        })
    }
}
