use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;

use crate::Addr;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// https://docs.rs/http-body-util/latest/http_body_util/combinators/struct.UnsyncBoxBody.html
pub type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, BoxError>;

pub type HttpRequest = http::Request<Body>;
pub type HttpResponse = http::Response<Body>;

/// An empty [`Body`].
pub fn empty_body() -> Body {
    use http_body_util::BodyExt;
    http_body_util::Empty::<Bytes>::new()
        .map_err(BoxError::from)
        .boxed_unsync()
}

// this will be probably only implemented in http/grpc stack
// or more protocols will be added, like jobs, etc
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Grpc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    AuthenticationRequired,
    AccessDenied,
    RateLimited,
}

impl Rejection {
    /// The response carries an HTTP status and the rejection for other protocol encoders.
    pub fn into_response(self) -> HttpResponse {
        let mut response = HttpResponse::new(empty_body());
        *response.status_mut() = match self {
            Self::AuthenticationRequired => http::StatusCode::UNAUTHORIZED,
            Self::AccessDenied => http::StatusCode::FORBIDDEN,
            Self::RateLimited => http::StatusCode::TOO_MANY_REQUESTS,
        };
        response.extensions_mut().insert(self);
        response
    }
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub remote: Addr,
    pub server: Addr,
    pub https: bool,
    pub received_at: f64,
}

pub trait Handler: Send + Sync + 'static {
    fn call<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, HttpResponse>;
}

/// A middleware that rebuilds the request must keep the request extensions. It must not
/// retain them past the call: they carry [`Peer`] and private state that counts the request in flight.
pub trait Middleware: Send + Sync + 'static {
    fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse>;
}

pub struct Next {
    chain: Arc<[Arc<dyn Middleware>]>,
    index: usize,
    handler: Arc<dyn Handler>,
}

impl Next {
    pub fn new(chain: Arc<[Arc<dyn Middleware>]>, handler: Arc<dyn Handler>) -> Self {
        Self {
            chain,
            index: 0,
            handler,
        }
    }

    pub async fn run(self, req: HttpRequest) -> HttpResponse {
        match self.chain.get(self.index) {
            Some(mw) => {
                let mw = Arc::clone(mw);
                let next = Self {
                    index: self.index + 1,
                    ..self
                };
                mw.handle(req, next).await
            }
            None => self.handler.call(req).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_rejections_keep_http_status_and_protocol_intent() {
        struct Case {
            name: &'static str,
            rejection: Rejection,
            status: u16,
        }
        let cases = [
            Case {
                name: "authentication_required",
                rejection: Rejection::AuthenticationRequired,
                status: 401,
            },
            Case {
                name: "access_denied",
                rejection: Rejection::AccessDenied,
                status: 403,
            },
            Case {
                name: "rate_limited",
                rejection: Rejection::RateLimited,
                status: 429,
            },
        ];
        for case in cases {
            let response = case.rejection.into_response();
            assert_eq!(response.status().as_u16(), case.status, "{}", case.name);
            assert_eq!(
                response.extensions().get::<Rejection>(),
                Some(&case.rejection),
                "{}",
                case.name
            );
        }
    }

    struct Tag(&'static str);

    impl Middleware for Tag {
        fn handle<'a>(&'a self, mut req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move {
                req.headers_mut()
                    .append("x-trace", format!("{}-in", self.0).parse().unwrap());
                let mut res = next.run(req).await;
                res.headers_mut()
                    .append("x-trace", format!("{}-out", self.0).parse().unwrap());
                res
            })
        }
    }

    struct Deny;

    impl Middleware for Deny {
        fn handle<'a>(&'a self, _req: HttpRequest, _next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async {
                http::Response::builder()
                    .status(403)
                    .body(empty_body())
                    .unwrap()
            })
        }
    }

    struct Echo;

    impl Handler for Echo {
        fn call<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move {
                let mut res = http::Response::builder()
                    .status(200)
                    .body(empty_body())
                    .unwrap();
                for v in req.headers().get_all("x-trace") {
                    res.headers_mut().append("x-trace", v.clone());
                }
                res
            })
        }
    }

    fn trace(res: &HttpResponse) -> Vec<&str> {
        res.headers()
            .get_all("x-trace")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chain_runs_outermost_first_and_unwinds_in_reverse() {
        let chain: Arc<[Arc<dyn Middleware>]> = Arc::from(vec![
            Arc::new(Tag("a")) as Arc<dyn Middleware>,
            Arc::new(Tag("b")),
        ]);
        let next = Next::new(chain, Arc::new(Echo));
        let res = next.run(http::Request::new(empty_body())).await;
        assert_eq!(res.status(), 200);
        assert_eq!(trace(&res), ["a-in", "b-in", "b-out", "a-out"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn short_circuit_skips_downstream_and_the_handler() {
        let chain: Arc<[Arc<dyn Middleware>]> = Arc::from(vec![
            Arc::new(Tag("a")) as Arc<dyn Middleware>,
            Arc::new(Deny),
            Arc::new(Tag("never")),
        ]);
        let next = Next::new(chain, Arc::new(Echo));
        let res = next.run(http::Request::new(empty_body())).await;
        assert_eq!(res.status(), 403);
        assert_eq!(trace(&res), ["a-out"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_chain_reaches_the_handler_directly() {
        let next = Next::new(Arc::from(Vec::new()), Arc::new(Echo));
        let mut req = http::Request::new(empty_body());
        req.headers_mut().append("x-trace", "solo".parse().unwrap());
        let res = next.run(req).await;
        assert_eq!(res.status(), 200);
        assert_eq!(trace(&res), ["solo"]);
    }
}
