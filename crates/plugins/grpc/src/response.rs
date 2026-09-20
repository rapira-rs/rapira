use base64::Engine;
use connectrpc::{ConnectError, EncodedResponse, ErrorCode, ErrorDetail, Response};
use extension_api::{BoxError, HttpResponse, Rejected, Rejection, grpc};
use http::{HeaderMap, HeaderValue, header::CONTENT_TYPE};
use http_body_util::BodyExt;

use crate::metadata;

pub(crate) fn boxed(
    mut response: http::Response<connectrpc::service::ConnectRpcBody>,
) -> HttpResponse {
    if response
        .headers()
        .get("content-type")
        .is_some_and(|value| value == "application/grpc+proto")
    {
        for name in ["grpc-status", "grpc-message", "grpc-status-details-bin"] {
            response.headers_mut().remove(name);
        }
    }
    response.map(|body| body.map_err(BoxError::from).boxed_unsync())
}

#[derive(Clone)]
pub(crate) struct ErrorContext(Option<HeaderValue>);

impl ErrorContext {
    pub fn new(headers: &HeaderMap) -> Self {
        Self(headers.get(CONTENT_TYPE).cloned())
    }

    pub fn is_streaming(&self) -> bool {
        self.0
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .and_then(connectrpc::Protocol::detect_from_content_type)
            .is_some_and(|protocol| protocol.is_streaming)
    }

    pub fn response(&self, status: ConnectError) -> HttpResponse {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = &self.0 {
            headers.insert(CONTENT_TYPE, content_type.clone());
        }
        boxed(status.into_http_response(&headers))
    }
}

pub(crate) fn reply(
    reply: grpc::Reply,
    limit: usize,
    connect: bool,
) -> Result<EncodedResponse, ConnectError> {
    let headers = metadata::encode(reply.headers)?;
    let trailers = metadata::encode(reply.trailers)?;
    match reply.result {
        Ok(message) => {
            if message.len() > limit {
                return Err(ConnectError::resource_exhausted(
                    "Response message exceeds size limit",
                )
                .with_headers(headers)
                .with_trailers(trailers));
            }
            let mut response = Response::new(message.into());
            response.headers = headers;
            response.trailers = trailers;
            Ok(response)
        }
        Err(status) => Err(application(status, connect)
            .with_headers(headers)
            .with_trailers(trailers)),
    }
}

fn application(status: grpc::Status, connect: bool) -> ConnectError {
    let code = match ErrorCode::from_grpc_code(u32::from(status.code)) {
        Some(code) => code,
        None => return ConnectError::internal("Invalid application status"),
    };
    let mut error = ConnectError::new(code, status.message);
    for detail in status.details {
        let type_url = if connect {
            detail.type_url.rsplit('/').next().unwrap().to_owned()
        } else {
            detail.type_url
        };
        error = error.with_detail(ErrorDetail {
            type_url,
            value: Some(base64::engine::general_purpose::STANDARD_NO_PAD.encode(detail.value)),
            debug: None,
        });
    }
    error
}

pub(crate) fn host(error: anyhow::Error) -> ConnectError {
    tracing::error!(target: "grpc", "PHP execution failed: {error:#}");
    match error
        .downcast_ref::<Rejected>()
        .map(|rejected| rejected.status)
    {
        Some(429) => ConnectError::resource_exhausted("Worker capacity exhausted"),
        Some(503) => ConnectError::unavailable("Worker unavailable"),
        _ => ConnectError::internal("PHP execution failed"),
    }
}

pub(crate) fn rejection(mut response: HttpResponse, context: &ErrorContext) -> HttpResponse {
    let Some(rejection) = response.extensions_mut().remove::<Rejection>() else {
        return response;
    };
    let status = match rejection {
        Rejection::AuthenticationRequired => {
            ConnectError::unauthenticated("Authentication required")
        }
        Rejection::AccessDenied => ConnectError::permission_denied("Access denied"),
        Rejection::RateLimited => ConnectError::resource_exhausted("Rate limited"),
    };
    let mut forwarded = HeaderMap::new();
    for (name, value) in response.headers() {
        if !metadata::reserved(name.as_str()) {
            forwarded.append(name.clone(), value.clone());
        }
    }
    context.response(status.with_headers(forwarded))
}
