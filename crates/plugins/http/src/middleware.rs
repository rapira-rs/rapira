//! The types of the middleware chain and the Peer of a connection.

use std::convert::Infallible;

use rapira_sapi::Addr;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// https://docs.rs/http-body-util/latest/http_body_util/combinators/struct.UnsyncBoxBody.html
pub type Body = http_body_util::combinators::UnsyncBoxBody<hyper::body::Bytes, BoxError>;
pub type Request = http::Request<Body>;
pub type Response = http::Response<Body>;
pub type Service = tower::util::BoxCloneService<Request, Response, Infallible>;
/// One middleware: a layer over the plugin's inner service. Applied outermost first in config order.
///
/// A middleware that rebuilds the request must keep the request extensions. It must not
/// retain them past the call: they carry the peer and private state that counts the request in flight.
pub type Layer = tower::util::BoxCloneServiceLayer<Service, Request, Response, Infallible>;

/// The peer of the connection. It travels to the inner service in the private request state.
#[derive(Debug, Clone)]
pub struct Peer {
    pub remote: Addr,
    pub server: Addr,
    pub https: bool,
    pub received_at: f64,
}
