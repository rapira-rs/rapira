use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use extension_api::{BoxError, HttpRequest};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use prost::Message;
use tokio::sync::oneshot;
use tokio_stream::Stream;
use tonic::codec::{Codec, EncodeBody};
use tonic::{Status, Streaming};
use tonic_prost::ProstCodec;
use tonic_reflection::pb;
use tonic_reflection::server::Builder;
use tower::ServiceExt;
use tower::util::BoxCloneSyncService;

use crate::Config;
use crate::codec::EnvelopeBody;

type Service = BoxCloneSyncService<
    http::Request<tonic::body::Body>,
    http::Response<tonic::body::Body>,
    Infallible,
>;

pub(crate) struct Reflection {
    v1: Service,
    v1alpha: Service,
}

fn builder(config: &Config) -> Builder<'_> {
    let mut builder = Builder::configure()
        .include_reflection_service(false)
        .register_encoded_file_descriptor_set(config.registry.encoded_descriptors())
        .register_encoded_file_descriptor_set(tonic_reflection::pb::v1::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_reflection::pb::v1alpha::FILE_DESCRIPTOR_SET);
    for service in config.registry.services() {
        builder = builder.with_service_name(&service.name);
    }
    builder
        .with_service_name("grpc.reflection.v1.ServerReflection")
        .with_service_name("grpc.reflection.v1alpha.ServerReflection")
}

impl Reflection {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        // The forwarding decoder applies client limits before protobuf re-encoding.
        let v1 = builder(config)
            .build_v1()?
            .max_decoding_message_size(usize::MAX)
            .max_encoding_message_size(config.max_response_message_size);
        let v1alpha = builder(config)
            .build_v1alpha()?
            .max_decoding_message_size(usize::MAX)
            .max_encoding_message_size(config.max_response_message_size);
        Ok(Self {
            v1: Service::new(v1),
            v1alpha: Service::new(v1alpha),
        })
    }

    pub fn route(&self, path: &str) -> Option<Service> {
        match path {
            "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo" => Some(self.v1.clone()),
            "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo" => {
                Some(self.v1alpha.clone())
            }
            _ => None,
        }
    }

    pub async fn call(
        service: Service,
        req: HttpRequest,
        input_limit: usize,
    ) -> extension_api::HttpResponse {
        let (parts, body) = req.into_parts();
        let failure = Arc::new(OnceLock::new());
        let (cancel, cancelled) = oneshot::channel();
        let body =
            if parts.uri.path() == "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo" {
                validated::<pb::v1::ServerReflectionRequest>(
                    body,
                    input_limit,
                    Arc::clone(&failure),
                    cancelled,
                )
            } else {
                validated::<pb::v1alpha::ServerReflectionRequest>(
                    body,
                    input_limit,
                    Arc::clone(&failure),
                    cancelled,
                )
            };
        let response = service
            .oneshot(http::Request::from_parts(parts, body))
            .await
            .unwrap_or_else(|never| match never {});
        response.map(|inner| {
            ObservedResponse {
                inner,
                failure,
                _cancel: cancel,
            }
            .map_err(BoxError::from)
            .boxed_unsync()
        })
    }
}

fn validated<M: Message + Default + 'static>(
    body: extension_api::Body,
    input_limit: usize,
    failure: Arc<OnceLock<Status>>,
    cancelled: oneshot::Receiver<()>,
) -> tonic::body::Body {
    let mut codec = ProstCodec::<M, M>::default();
    let incoming = Streaming::new_request(
        codec.decoder(),
        EnvelopeBody::streaming(body),
        None,
        Some(input_limit),
    );
    let requests = ValidatedRequests {
        incoming: Some(incoming),
        failure,
        cancelled,
    };
    tonic::body::Body::new(EncodeBody::new_client(
        codec.encoder(),
        requests,
        None,
        None,
    ))
}

// Preserve decode failures when the upstream reflection service closes its response stream.
// https://github.com/grpc/grpc-rust/blob/a597b92070dc12d2239923821f86391ad857d910/tonic-reflection/src/server/v1.rs#L35-L39
struct ValidatedRequests<M> {
    incoming: Option<Streaming<M>>,
    failure: Arc<OnceLock<Status>>,
    cancelled: oneshot::Receiver<()>,
}

impl<M> Stream for ValidatedRequests<M> {
    type Item = Result<M, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.incoming.is_none() {
            return Poll::Ready(None);
        }
        if Pin::new(&mut self.cancelled).poll(cx).is_ready() {
            self.incoming = None;
            return Poll::Ready(None);
        }
        match ready!(Pin::new(self.incoming.as_mut().unwrap()).poll_next(cx)) {
            Some(Ok(message)) => Poll::Ready(Some(Ok(message))),
            Some(Err(status)) => {
                let _ = self.failure.set(status);
                self.incoming = None;
                Poll::Ready(None)
            }
            None => {
                self.incoming = None;
                Poll::Ready(None)
            }
        }
    }
}

struct ObservedResponse {
    inner: tonic::body::Body,
    failure: Arc<OnceLock<Status>>,
    // Dropping the response wakes a request reader that is waiting for more input.
    _cancel: oneshot::Sender<()>,
}

impl Body for ObservedResponse {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        match ready!(Pin::new(&mut self.inner).poll_frame(cx)) {
            Some(Ok(frame)) => match frame.into_trailers() {
                Ok(mut trailers) => {
                    if trailers
                        .get("grpc-status")
                        .is_some_and(|value| value == "0")
                        && let Some(status) = self.failure.get()
                    {
                        status.add_header(&mut trailers)?;
                    }
                    Poll::Ready(Some(Ok(Frame::trailers(trailers))))
                }
                Err(frame) => Poll::Ready(Some(Ok(frame))),
            },
            other => Poll::Ready(other),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}
