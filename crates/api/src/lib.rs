use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

mod middleware;
pub use middleware::{
    Body, BoxError, BoxFuture, Handler, HttpRequest, HttpResponse, Middleware, Next, Peer,
    Protocol, empty_body,
};
pub use rapira_net::{ListenAddr, PrepareCtx, PreparedListener};

pub type Result<T = (), E = anyhow::Error> = std::result::Result<T, E>;

/// Lifecycle: `init` → `prepare` (master-side, pre-fork) → `run` → `shutdown`; the host drops the in-flight `run` future before it calls `shutdown`.
pub trait Extension: Send + 'static {
    type Config;

    fn init(config: Self::Config) -> Self
    where
        Self: Sized;
    fn name(&self) -> &str;
    /// Master-side pre-fork hook: synchronous, no runtime exists
    fn prepare(&mut self, _ctx: &mut PrepareCtx) -> Result<()> {
        Ok(())
    }
    fn run(&mut self, php: Php) -> impl Future<Output = Result<()>> + Send;
    fn shutdown(&mut self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

#[doc(hidden)]
pub trait Backend: Send + Sync + 'static {
    fn exec(&self, req: Request) -> Pin<Box<dyn Future<Output = Result<Reply>> + Send + '_>>;
    fn unary(
        &self,
        call: UnaryCall,
    ) -> Pin<Box<dyn Future<Output = Result<Option<UnaryReply>>> + Send + '_>>;
}

pub enum ReplyEvent {
    Interim {
        status: u16,
        headers: http::HeaderMap,
    },
    Head {
        status: u16,
        headers: http::HeaderMap,
        content_length: Option<u64>,
        bodiless: bool,
    },
    Chunk(bytes::Bytes),
    File {
        file: std::fs::File,
        offset: u64,
        /// Never zero: a producer does not emit an empty slice.
        len: u64,
    },
    End {
        trailers: http::HeaderMap,
        truncated: bool,
    },
}

pub trait ReplySource: Send + 'static {
    fn poll_next(&mut self, cx: &mut std::task::Context<'_>)
    -> std::task::Poll<Option<ReplyEvent>>;
}

pub struct Reply(Box<dyn ReplySource>);
impl Reply {
    pub fn new(source: Box<dyn ReplySource>) -> Self {
        Self(source)
    }

    pub fn poll_next(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<ReplyEvent>> {
        self.0.poll_next(cx)
    }

    pub async fn next(&mut self) -> Option<ReplyEvent> {
        std::future::poll_fn(|cx| self.0.poll_next(cx)).await
    }
}

/// Every clone shares the host's backend handle: never keep a spare past `run`/`shutdown`, the host's shutdown contract needs them all dropped.
#[derive(Clone)]
pub struct Php {
    backend: Arc<dyn Backend>,
}

impl Php {
    #[doc(hidden)]
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    /// A pre-dispatch refusal errors with a downcastable [`Rejected`]; response-shape failures surface from [`Reply::next`].
    pub async fn exec(&self, req: Request) -> Result<Reply> {
        self.backend.exec(req).await
    }

    /// `Err` is a refusal before dispatch: PHP never saw the call. `Ok(None)` means the worker dropped the call without an outcome. Dropping the future cancels the call.
    pub async fn unary(&self, call: UnaryCall) -> Result<Option<UnaryReply>> {
        self.backend.unary(call).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Inet(std::net::SocketAddr),
    Unix(Option<PathBuf>),
}

#[derive(Debug, Clone)]
pub struct ClientCert {
    pub serial: String,
    pub organization: Option<String>,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct Tls {
    pub version: String,
    pub cipher: String,
    /// PHP `Tls::$negotiatedProtocol`.
    pub alpn: Option<String>,
    /// PHP `Tls::$requestedServerName`.
    pub server_name: Option<String>,
    pub cert: Option<ClientCert>,
}

#[derive(Debug)]
pub struct Rejected {
    pub status: u16,
    pub reason: String,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.reason)
    }
}

impl std::error::Error for Rejected {}

/// An extension passes `None` for a field that its protocol does not carry.
pub struct Request {
    pub method: String,
    pub uri: String,
    pub target: Option<Vec<u8>>,
    pub authority: Option<Vec<u8>>,
    pub https: bool,
    pub protocol: String,
    pub remote: Addr,
    pub server: Addr,
    pub server_name: String,
    pub server_port: u16,
    pub tls: Option<Tls>,
    pub received_at: Option<f64>,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

/// One unary RPC. `message` is the binary protobuf encoding of the method's input message.
pub struct UnaryCall {
    /// `package.Service/Method`, without a leading slash.
    pub method: String,
    pub protocol: RpcProtocol,
    /// The request headers as received. rapira_sapi drops the transport names and decodes `-bin` values when it builds `Context::$metadata`.
    pub metadata: http::HeaderMap,
    /// Unix seconds.
    pub deadline: Option<f64>,
    pub remote: Addr,
    pub message: bytes::Bytes,
}

/// The protocol that the client of an RPC used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcProtocol {
    Grpc,
    GrpcWeb,
    Connect,
}

/// The outcome of a unary RPC. The metadata is in wire form: `-bin` values are unpadded base64.
pub struct UnaryReply {
    pub headers: http::HeaderMap,
    pub trailers: http::HeaderMap,
    /// The output message, or the status the call failed with.
    pub outcome: std::result::Result<bytes::Bytes, RpcStatus>,
}

/// `google.rpc.Status`. `code` is 1..=16. Each detail is a (type URL, packed message) pair. https://github.com/googleapis/googleapis/blob/master/google/rpc/status.proto
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcStatus {
    pub code: u32,
    pub message: String,
    pub details: Vec<(String, bytes::Bytes)>,
}
