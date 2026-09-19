use extension_api::{BoxError, HttpResponse, Rejected, Rejection, grpc};
use http::HeaderMap;
use http_body_util::BodyExt;
use prost::Message;
use tonic::{Code, Status};

pub(crate) struct Metadata {
    pub headers: HeaderMap,
    pub trailers: HeaderMap,
}

pub(crate) fn boxed(response: http::Response<tonic::body::Body>) -> HttpResponse {
    response.map(|body| body.map_err(BoxError::from).boxed_unsync())
}

pub(crate) fn error(status: Status) -> HttpResponse {
    boxed(status.into_http())
}

pub(crate) fn attach(
    mut response: http::Response<tonic::body::Body>,
    mut metadata: Metadata,
) -> HttpResponse {
    for name in ["grpc-status", "grpc-message", "grpc-status-details-bin"] {
        if let Some(value) = response.headers_mut().remove(name) {
            metadata.trailers.insert(name, value);
        }
    }
    for (name, value) in &metadata.headers {
        response.headers_mut().append(name.clone(), value.clone());
    }
    response.map(|body| {
        body.with_trailers(std::future::ready(Some(Ok::<_, Status>(metadata.trailers))))
            .map_err(BoxError::from)
            .boxed_unsync()
    })
}

#[derive(Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<prost_types::Any>,
}

pub(crate) fn application(status: grpc::Status) -> Status {
    if !(1..=16).contains(&status.code) {
        return Status::internal("Invalid application status");
    }
    let code = Code::from_i32(i32::from(status.code));
    if status.details.is_empty() {
        return Status::new(code, status.message);
    }
    let rich = RpcStatus {
        code: i32::from(status.code),
        message: status.message.clone(),
        details: status
            .details
            .into_iter()
            .map(|detail| prost_types::Any {
                type_url: detail.type_url,
                value: detail.value.to_vec(),
            })
            .collect(),
    };
    Status::with_details(code, status.message, rich.encode_to_vec().into())
}

pub(crate) fn host(error: anyhow::Error) -> Status {
    tracing::error!(target: "grpc", "PHP execution failed: {error:#}");
    match error
        .downcast_ref::<Rejected>()
        .map(|rejected| rejected.status)
    {
        Some(429) => Status::resource_exhausted("Worker capacity exhausted"),
        Some(503) => Status::unavailable("Worker unavailable"),
        _ => Status::internal("PHP execution failed"),
    }
}

pub(crate) fn rejection(mut response: HttpResponse) -> HttpResponse {
    let Some(rejection) = response.extensions_mut().remove::<Rejection>() else {
        return response;
    };
    let status = match rejection {
        Rejection::AuthenticationRequired => Status::unauthenticated("Authentication required"),
        Rejection::AccessDenied => Status::permission_denied("Access denied"),
        Rejection::RateLimited => Status::resource_exhausted("Rate limited"),
    };
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers() {
        if !crate::metadata::reserved(name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }
    attach(
        status.into_http(),
        Metadata {
            headers,
            trailers: HeaderMap::new(),
        },
    )
}
