//! Push-based HTTP middleware contract for Rapira.
//!
//! Each middleware declares its active phases at boot so unused phases cost one bit check.
//! Request hooks run outer to inner on the IO shard; response-head hooks run inner to outer
//! and apply only to outer middlewares `0..idx` when middleware `idx` short-circuits.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Extensions, HeaderMap, Method, Uri, Version};

/// Network address of a connection peer or listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAddr {
    Inet(SocketAddr),
    Unix(Option<PathBuf>),
}

/// Connection peer metadata attached to each request.
#[derive(Debug, Clone)]
pub struct Peer {
    pub remote: PeerAddr,
    pub server: PeerAddr,
    pub https: bool,
    pub received_at: f64,
}

/// Bitmask of lifecycle phases a middleware participates in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Phases(u8);

impl Phases {
    pub const NONE: Self = Self(0);
    pub const REQUEST: Self = Self(1 << 0);
    pub const RESPONSE_HEAD: Self = Self(1 << 1);

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// Mutable request context passed through Phase-1 middleware on the IO shard.
#[derive(Debug)]
pub struct RequestCtx {
    pub method: Method,
    pub uri: Uri,
    pub version: Version,
    pub headers: HeaderMap,
    pub peer: Peer,
    pub extensions: Extensions,
}

/// Mutable response status and headers passed to Phase-2 hooks before wire framing.
#[derive(Clone, Debug)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: HeaderMap,
    /// Known uncompressed body length when available before streaming begins.
    pub known_len: Option<u64>,
}

impl ResponseHead {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            known_len: None,
        }
    }
}

/// Representation metadata for a `304 Not Modified` response whose wire headers omit `Content-Type` or `Content-Length`.
#[derive(Clone, Debug)]
pub struct RepresentationMeta {
    pub content_type: http::header::HeaderValue,
    pub known_len: Option<u64>,
}

/// Push-based body transform (for example gzip or zstd compression).
pub trait BodyEncoder: Send {
    fn write(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> io::Result<()>;

    /// Flushes buffered encoded bytes after a streamed chunk so clients receive incremental output.
    fn flush(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let _ = out;
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> io::Result<()>;
}

/// Asynchronous byte stream for short-circuit responses that stream beyond the initial chunk.
pub trait BodyStream: Send + Unpin {
    fn poll_next_chunk(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Bytes>>>;
}

/// Response returned directly by a Phase-1 middleware without reaching PHP.
pub struct ShortResponse {
    pub head: ResponseHead,
    /// Complete body for full responses, or eager first chunk for streamed responses.
    pub body: Bytes,
    /// Remaining chunks when the response streams past `body`.
    pub rest: Option<Box<dyn BodyStream>>,
}

impl ShortResponse {
    pub fn full(head: ResponseHead, body: Bytes) -> Self {
        Self {
            head,
            body,
            rest: None,
        }
    }
}

/// Synchronous fast path or async future returned by `Middleware::on_request`.
pub enum RequestStep<'a> {
    Next,
    Respond(ShortResponse),
    Async(Pin<Box<dyn Future<Output = Option<ShortResponse>> + Send + 'a>>),
}

/// Host-side HTTP middleware stage applied in config order with index 0 outermost.
pub trait Middleware: Send + Sync + 'static {
    fn phases(&self) -> Phases;

    fn on_request<'a>(&'a self, req: &'a mut RequestCtx) -> RequestStep<'a> {
        let _ = req;
        RequestStep::Next
    }

    fn on_response_head(
        &self,
        is_head: bool,
        extensions: &Extensions,
        head: &mut ResponseHead,
    ) -> Option<Box<dyn BodyEncoder>> {
        let _ = (is_head, extensions, head);
        None
    }
}

/// Compiled middleware list tracking the union of active phases.
#[derive(Default)]
pub struct MiddlewareChain {
    all: Vec<Arc<dyn Middleware>>,
    active: Phases,
}

impl MiddlewareChain {
    pub fn new(middlewares: Vec<Arc<dyn Middleware>>) -> Self {
        let active = middlewares
            .iter()
            .fold(Phases::NONE, |acc, mw| acc.union(mw.phases()));
        Self {
            all: middlewares,
            active,
        }
    }

    #[inline]
    pub fn has_request(&self) -> bool {
        self.active.contains(Phases::REQUEST)
    }

    /// Runs Phase-1 hooks in config order (outer -> inner).
    /// Returns `Some((outer_depth, response))` when middleware `outer_depth` short-circuits.
    pub async fn run_request(&self, req: &mut RequestCtx) -> Option<(usize, ShortResponse)> {
        for (idx, mw) in self.all.iter().enumerate() {
            if !mw.phases().contains(Phases::REQUEST) {
                continue;
            }
            match mw.on_request(req) {
                RequestStep::Next => {}
                RequestStep::Respond(response) => return Some((idx, response)),
                RequestStep::Async(fut) => {
                    if let Some(response) = fut.await {
                        return Some((idx, response));
                    }
                }
            }
        }
        None
    }

    /// Runs Phase-2 hooks inner -> outer for all middlewares (or strictly below `outer_depth` on short-circuit).
    /// Returns any active `BodyEncoder`s in inner -> outer order.
    pub fn run_response_head(
        &self,
        outer_depth: Option<usize>,
        is_head: bool,
        extensions: &Extensions,
        head: &mut ResponseHead,
    ) -> Vec<Box<dyn BodyEncoder>> {
        if !self.active.contains(Phases::RESPONSE_HEAD) {
            return Vec::new();
        }
        let mut encoders = Vec::new();
        let limit = outer_depth.map_or(self.all.len(), |d| d.min(self.all.len()));
        for mw in self.all[..limit].iter().rev() {
            if mw.phases().contains(Phases::RESPONSE_HEAD)
                && let Some(enc) = mw.on_response_head(is_head, extensions, head)
            {
                encoders.push(enc);
            }
        }
        encoders
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{HeaderName, HeaderValue};
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    fn test_ctx() -> RequestCtx {
        RequestCtx {
            method: Method::GET,
            uri: Uri::from_static("/hello"),
            version: Version::HTTP_11,
            headers: HeaderMap::new(),
            peer: Peer {
                remote: PeerAddr::Inet(([127, 0, 0, 1], 40000).into()),
                server: PeerAddr::Inet(([127, 0, 0, 1], 8000).into()),
                https: false,
                received_at: 0.0,
            },
            extensions: Extensions::new(),
        }
    }

    struct TraceMw {
        name: &'static str,
        short_circuit: bool,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Middleware for TraceMw {
        fn phases(&self) -> Phases {
            Phases::REQUEST.union(Phases::RESPONSE_HEAD)
        }

        fn on_request<'a>(&'a self, req: &'a mut RequestCtx) -> RequestStep<'a> {
            req.headers.append(
                HeaderName::from_static("x-trace"),
                HeaderValue::from_str(&format!("{}-in", self.name)).unwrap(),
            );
            self.log.lock().unwrap().push(format!("{}-req", self.name));
            if self.short_circuit {
                return RequestStep::Respond(ShortResponse::full(
                    ResponseHead::new(204),
                    Bytes::new(),
                ));
            }
            RequestStep::Next
        }

        fn on_response_head(
            &self,
            _is_head: bool,
            _extensions: &Extensions,
            head: &mut ResponseHead,
        ) -> Option<Box<dyn BodyEncoder>> {
            head.headers.append(
                HeaderName::from_static("x-trace"),
                HeaderValue::from_str(&format!("{}-out", self.name)).unwrap(),
            );
            self.log.lock().unwrap().push(format!("{}-head", self.name));
            None
        }
    }

    fn poll_ready<F: Future>(fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("expected synchronous completion"),
        }
    }

    #[test]
    fn onion_order_runs_outer_to_inner_then_inner_to_outer() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = MiddlewareChain::new(vec![
            Arc::new(TraceMw {
                name: "outer",
                short_circuit: false,
                log: Arc::clone(&log),
            }),
            Arc::new(TraceMw {
                name: "inner",
                short_circuit: false,
                log: Arc::clone(&log),
            }),
        ]);
        let mut ctx = test_ctx();
        assert!(poll_ready(chain.run_request(&mut ctx)).is_none());
        let mut head = ResponseHead::new(200);
        chain.run_response_head(None, false, &ctx.extensions, &mut head);

        assert_eq!(
            *log.lock().unwrap(),
            vec!["outer-req", "inner-req", "inner-head", "outer-head"]
        );
    }

    #[test]
    fn short_circuit_unwinds_only_outer_middlewares() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = MiddlewareChain::new(vec![
            Arc::new(TraceMw {
                name: "outer",
                short_circuit: false,
                log: Arc::clone(&log),
            }),
            Arc::new(TraceMw {
                name: "mid",
                short_circuit: true,
                log: Arc::clone(&log),
            }),
            Arc::new(TraceMw {
                name: "inner",
                short_circuit: false,
                log: Arc::clone(&log),
            }),
        ]);
        let mut ctx = test_ctx();
        let (outer_depth, mut resp) =
            poll_ready(chain.run_request(&mut ctx)).expect("short circuit");
        assert_eq!(outer_depth, 1);
        chain.run_response_head(Some(outer_depth), false, &ctx.extensions, &mut resp.head);

        assert_eq!(
            *log.lock().unwrap(),
            vec!["outer-req", "mid-req", "outer-head"]
        );
    }
}
