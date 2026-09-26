use std::convert::Infallible;

use connectrpc::ConnectRpcBody;

/// The request that `serve_connection` hands to the service.
pub type Request = http::Request<hyper::body::Incoming>;
/// The response of `ConnectRpcService` after `status_in_trailers`.
pub type Response = http::Response<ConnectRpcBody>;
pub type Service = tower::util::BoxCloneService<Request, Response, Infallible>;
/// One interceptor: a layer over the connectrpc service. Applied outermost first in config order.
pub type Interceptor = tower::util::BoxCloneServiceLayer<Service, Request, Response, Infallible>;
