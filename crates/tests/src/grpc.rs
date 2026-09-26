//! Clients that speak gRPC, gRPC-Web and Connect over the wire.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rapira_net::ListenAddr;
use rapira_sapi::Addr;
use tokio::io::{AsyncRead, AsyncWrite};

pub const ECHO_SERVICE: &str = "rapira.test.v1.EchoService";
pub const ECHO_PATH: &str = "/rapira.test.v1.EchoService/Echo";

/// The type URL of the detail that a failed call usually carries.
pub const ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// `EchoRequest { text: "hi" }`.
pub const HI: &[u8] = &[0x0a, 0x02, 0x68, 0x69];

/// [`HI`] in an uncompressed gRPC envelope.
pub const HI_FRAME: &[u8] = &[0x00, 0x00, 0x00, 0x00, 0x04, 0x0a, 0x02, 0x68, 0x69];

pub use crate::{Fields, fields};

/// The `google.rpc.Status` of NOT_FOUND `no invoice` with one detail: `url` packing `0a 01 78`.
///
/// Sources: `google/rpc/status.proto` (code 1, message 2, details 3) and `google/protobuf/any.proto` (type_url 1, value 2).
pub fn status_bytes(url: &str) -> Vec<u8> {
    let any = [
        &[0x0a, url.len() as u8][..],
        url.as_bytes(),
        &[0x12, 0x03, 0x0a, 0x01, 0x78],
    ]
    .concat();
    assert!(any.len() < 128, "the lengths must fit one varint byte");
    [
        &[0x08, 0x05, 0x12, 0x0a][..],
        b"no invoice",
        &[0x1a, any.len() as u8],
        &any,
    ]
    .concat()
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

#[derive(Clone, Copy, Debug)]
pub enum Wire {
    /// HTTP/2 with prior knowledge.
    H2,
    Http1,
}

enum Sender {
    H2(hyper::client::conn::http2::SendRequest<Full<Bytes>>),
    Http1(hyper::client::conn::http1::SendRequest<Full<Bytes>>),
}

/// One client connection.
pub struct Conn {
    sender: Sender,
    /// The remote address that the server sees for this client.
    pub peer: Addr,
}

/// A collected response.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub trailers: HeaderMap,
}

impl Response {
    /// The `grpc-status` trailer, or the header of a trailers-only response.
    pub fn grpc_status(&self) -> Option<&str> {
        self.field("grpc-status")
    }

    /// The `grpc-message` trailer, or the header of a trailers-only response.
    pub fn grpc_message(&self) -> Option<&str> {
        self.field("grpc-message")
    }

    fn field(&self, name: &str) -> Option<&str> {
        self.trailers
            .get(name)
            .or_else(|| self.headers.get(name))
            .and_then(|v| v.to_str().ok())
    }
}

impl Conn {
    pub async fn open(listen: &ListenAddr, wire: Wire) -> anyhow::Result<Conn> {
        let (io, peer): (Box<dyn Io>, Addr) = match listen {
            ListenAddr::Tcp(addr) => {
                let stream = tokio::net::TcpStream::connect(addr).await?;
                let local = stream.local_addr()?;
                (Box::new(stream), Addr::Inet(local))
            }
            ListenAddr::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path).await?;
                (Box::new(stream), Addr::Unix(None))
            }
        };
        let io = TokioIo::new(io);
        let sender = match wire {
            Wire::H2 => {
                let (send, conn) =
                    hyper::client::conn::http2::handshake(TokioExecutor::new(), io).await?;
                tokio::spawn(conn);
                Sender::H2(send)
            }
            Wire::Http1 => {
                let (send, conn) = hyper::client::conn::http1::handshake(io).await?;
                tokio::spawn(conn);
                Sender::Http1(send)
            }
        };
        Ok(Conn { sender, peer })
    }

    pub async fn send(
        &mut self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> anyhow::Result<Response> {
        let (parts, body) = self
            .request(method, path, headers, body)
            .await?
            .into_parts();
        let collected = body.collect().await?;
        let trailers = collected.trailers().cloned().unwrap_or_default();
        Ok(Response {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body: collected.to_bytes(),
            trailers,
        })
    }

    /// Sends a request and returns when the response head arrives. The body streams.
    pub async fn request(
        &mut self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> anyhow::Result<http::Response<hyper::body::Incoming>> {
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
        Ok(match &mut self.sender {
            Sender::H2(send) => send.send_request(req).await?,
            Sender::Http1(send) => send.send_request(req).await?,
        })
    }

    /// A gRPC call over this connection.
    pub async fn grpc(
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

/// `message` in an uncompressed gRPC envelope.
pub fn envelope(message: &[u8]) -> Vec<u8> {
    let mut out = vec![0];
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// The fields of the gRPC-Web trailer frame (flag 0x80) in `body`.
pub fn web_trailers(mut body: &[u8]) -> HeaderMap {
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
pub fn status_details(fields: &HeaderMap) -> Option<Vec<u8>> {
    let value = fields.get("grpc-status-details-bin")?;
    STANDARD_NO_PAD.decode(value.as_bytes()).ok()
}
