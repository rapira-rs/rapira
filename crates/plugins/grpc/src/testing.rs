//! Test doubles and wire clients for the in-crate tests.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use bytes::Bytes;
use extension_api::{
    Addr, Backend, Extension, ListenAddr, Php, PrepareCtx, Reply, Request, Result, RpcStatus,
    UnaryCall, UnaryReply,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{Config, Schema, Server};

pub(crate) const ECHO: &str = "rapira.test.v1.EchoService";

/// `EchoRequest { text: "hi" }`.
pub(crate) const HI: &[u8] = &[0x0a, 0x02, 0x68, 0x69];

/// [`HI`] in an uncompressed gRPC envelope.
pub(crate) const HI_FRAME: &[u8] = &[0x00, 0x00, 0x00, 0x00, 0x04, 0x0a, 0x02, 0x68, 0x69];

pub(crate) type Fields = &'static [(&'static str, &'static str)];

pub(crate) fn fields(lines: Fields) -> HeaderMap {
    lines
        .iter()
        .map(|&(k, v)| (HeaderName::from_static(k), HeaderValue::from_static(v)))
        .collect()
}

pub(crate) fn schema() -> Arc<Schema> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/echo.binpb");
    Arc::new(Schema::load(&path, &[ECHO.to_owned()]).expect("echo.binpb loads"))
}

pub(crate) fn config(listen: ListenAddr) -> Config {
    Config {
        listen,
        schema: schema(),
        default_timeout: None,
        max_timeout: None,
        drain_grace: Duration::from_secs(5),
    }
}

pub(crate) fn tcp() -> ListenAddr {
    ListenAddr::Tcp(([127, 0, 0, 1], 0).into())
}

/// What the fake PHP does with a call.
#[derive(Clone, Copy)]
pub(crate) enum Answer {
    /// Replies with the request message.
    Echo,
    /// Fails with NOT_FOUND and one `ErrorInfo` detail. Headers `x-h: v`, trailers `x-t: w` and `x-b-bin: AQI`.
    Fail,
    /// Replies with the request message after the delay.
    Late(Duration),
    /// Never replies.
    Never,
}

/// A PHP pool that answers every call the same way.
pub(crate) struct FakePhp {
    answer: Answer,
    /// The calls that reached PHP, in order.
    pub(crate) calls: Mutex<Vec<UnaryCall>>,
    /// The host dropped a call that PHP never answers.
    pub(crate) dropped: AtomicBool,
}

impl FakePhp {
    pub(crate) fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            calls: Mutex::new(Vec::new()),
            dropped: AtomicBool::new(false),
        })
    }

    pub(crate) fn seen(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Backend for FakePhp {
    fn exec(&self, _req: Request) -> Pin<Box<dyn Future<Output = Result<Reply>> + Send + '_>> {
        unreachable!("the gRPC plugin sends no HTTP request")
    }

    fn unary(
        &self,
        call: UnaryCall,
    ) -> Pin<Box<dyn Future<Output = Result<Option<UnaryReply>>> + Send + '_>> {
        let message = call.message.clone();
        self.calls.lock().unwrap().push(call);
        Box::pin(async move {
            let outcome = match self.answer {
                Answer::Echo => Ok(message),
                Answer::Late(delay) => {
                    tokio::time::sleep(delay).await;
                    Ok(message)
                }
                Answer::Never => {
                    let _dropped = SetOnDrop(&self.dropped);
                    std::future::pending().await
                }
                Answer::Fail => {
                    return Ok(Some(UnaryReply {
                        headers: fields(&[("x-h", "v")]),
                        trailers: fields(&[("x-t", "w"), ("x-b-bin", "AQI")]),
                        outcome: Err(RpcStatus {
                            code: 5,
                            message: "no invoice".into(),
                            details: vec![(
                                "type.googleapis.com/google.rpc.ErrorInfo".into(),
                                Bytes::from_static(&[0x0a, 0x01, 0x78]),
                            )],
                        }),
                    }));
                }
            };
            Ok(Some(UnaryReply {
                headers: HeaderMap::new(),
                trailers: HeaderMap::new(),
                outcome,
            }))
        })
    }
}

/// A server whose `run` future is gone, as after a cancelled run. The thread serves on.
pub(crate) struct Running {
    pub(crate) server: Server,
    pub(crate) listen: ListenAddr,
}

pub(crate) async fn start(config: Config, backend: Arc<dyn Backend>) -> Running {
    let mut server = Server::init(config);
    server.prepare(&mut PrepareCtx::new()).expect("bind");
    let listen = server.prepared.as_ref().expect("prepared").addr().clone();
    let mut run = Box::pin(server.run(Php::new(backend)));
    // One poll starts the server thread.
    assert!(std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx).is_pending())).await);
    drop(run);
    Running { server, listen }
}

/// Polls `done` for at most 10 s.
pub(crate) async fn wait_until(done: impl Fn() -> bool) {
    for _ in 0..1000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition did not hold within 10 s");
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Wire {
    /// HTTP/2 with prior knowledge.
    H2,
    Http1,
}

enum Sender {
    H2(hyper::client::conn::http2::SendRequest<Full<Bytes>>),
    Http1(hyper::client::conn::http1::SendRequest<Full<Bytes>>),
}

/// One client connection.
pub(crate) struct Conn {
    sender: Sender,
    /// The remote address that the server sees for this client.
    pub(crate) peer: Addr,
}

/// A collected response.
#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
    pub(crate) trailers: HeaderMap,
}

impl Conn {
    pub(crate) async fn open(listen: &ListenAddr, wire: Wire) -> Conn {
        let (io, peer): (Box<dyn Io>, Addr) = match listen {
            ListenAddr::Tcp(addr) => {
                let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
                let local = stream.local_addr().expect("local addr");
                (Box::new(stream), Addr::Inet(local))
            }
            ListenAddr::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .expect("connect");
                (Box::new(stream), Addr::Unix(None))
            }
        };
        let io = TokioIo::new(io);
        let sender = match wire {
            Wire::H2 => {
                let (send, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
                    .await
                    .expect("h2 handshake");
                tokio::spawn(conn);
                Sender::H2(send)
            }
            Wire::Http1 => {
                let (send, conn) = hyper::client::conn::http1::handshake(io)
                    .await
                    .expect("http/1.1 handshake");
                tokio::spawn(conn);
                Sender::Http1(send)
            }
        };
        Conn { sender, peer }
    }

    pub(crate) async fn send(
        &mut self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> anyhow::Result<Response> {
        let uri = match self.sender {
            Sender::H2(_) => format!("http://localhost{path}"),
            Sender::Http1(_) => path.to_owned(),
        };
        let mut req = http::Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost");
        for &(k, v) in headers {
            req = req.header(k, v);
        }
        let req = req.body(Full::new(Bytes::copy_from_slice(body)))?;
        let resp = match &mut self.sender {
            Sender::H2(send) => send.send_request(req).await?,
            Sender::Http1(send) => send.send_request(req).await?,
        };
        let (parts, body) = resp.into_parts();
        let collected = body.collect().await?;
        let trailers = collected.trailers().cloned().unwrap_or_default();
        Ok(Response {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body: collected.to_bytes(),
            trailers,
        })
    }

    /// A gRPC call over this connection.
    pub(crate) async fn grpc(
        &mut self,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> anyhow::Result<Response> {
        let mut all = vec![("content-type", "application/grpc"), ("te", "trailers")];
        all.extend_from_slice(headers);
        self.send(Method::POST, path, &all, body).await
    }
}

/// The `grpc-status` trailer, or the header of a trailers-only response.
pub(crate) fn grpc_status(r: &Response) -> Option<&str> {
    r.trailers
        .get("grpc-status")
        .or_else(|| r.headers.get("grpc-status"))
        .and_then(|v| v.to_str().ok())
}

/// The fields of the gRPC-Web trailer frame (flag 0x80) in `body`.
pub(crate) fn web_trailers(mut body: &[u8]) -> HeaderMap {
    let mut out = HeaderMap::new();
    while body.len() >= 5 {
        let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
        let (frame, rest) = body[5..].split_at(len);
        if body[0] & 0x80 != 0 {
            for line in frame.split(|&b| b == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if let Some(at) = line.iter().position(|&b| b == b':') {
                    let name = HeaderName::from_bytes(&line[..at]).expect("trailer name");
                    let value = HeaderValue::from_bytes(line[at + 1..].trim_ascii())
                        .expect("trailer value");
                    out.append(name, value);
                }
            }
        }
        body = rest;
    }
    out
}

/// The `google.rpc.Status` bytes of a `grpc-status-details-bin` field.
pub(crate) fn status_details(fields: &HeaderMap) -> Option<Vec<u8>> {
    let value = fields.get("grpc-status-details-bin")?;
    STANDARD_NO_PAD.decode(value.as_bytes()).ok()
}
