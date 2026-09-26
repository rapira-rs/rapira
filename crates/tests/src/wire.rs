//! An HTTP/1.1 client that sends a `rapira_sapi::Request` to a server and reads the response back as `Frame`s.
//!
//! The request carries the method, the target (else the `uri`), `Host` (from `authority`, else `localhost` on HTTP/1.1), the header lines in order, `Content-Type` from `content_type` when no header line sets it, and the raw body.
//! The server sets the other `Request` fields itself: `remote`, `server`, `server_name`, `server_port`, `https`, `tls`, `received_at` and `content_length`.

use std::net::SocketAddr;
use std::sync::OnceLock;

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST};
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, Version};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use rapira_sapi::types::Body;
use rapira_sapi::{Frame, Request, ResponseHead};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// The connections run here, so the frames arrive while a sync caller blocks on the receiver.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// [`submit_async`] for a caller outside a tokio runtime.
pub fn submit(addr: SocketAddr, req: Request) -> anyhow::Result<mpsc::Receiver<Frame>> {
    runtime().block_on(submit_async(addr, req))
}

/// Sends `req` to `addr` on a new connection and returns once the connection is open.
/// The receiver gets `Head`, one `Chunk` per body frame, and `End`; a body error is `End { truncated: true }`, and no frame means that no response head arrived.
/// Dropping the receiver closes the connection at once.
pub async fn submit_async(addr: SocketAddr, req: Request) -> anyhow::Result<mpsc::Receiver<Frame>> {
    let request = to_http(req)?;
    let head_only = request.method() == Method::HEAD;
    let stream = runtime().spawn(TcpStream::connect(addr)).await??;
    let (tx, rx) = mpsc::channel(16);
    runtime().spawn(async move {
        tokio::select! {
            () = tx.closed() => {}
            () = exchange(stream, request, head_only, &tx) => {}
        }
    });
    Ok(rx)
}

fn to_http(req: Request) -> anyhow::Result<http::Request<Full<Bytes>>> {
    let Body::Raw(body) = req.body else {
        anyhow::bail!("a multipart body cannot travel: send its encoded bytes as Body::Raw");
    };
    let uri = match req.target {
        Some(target) => Uri::try_from(target)?,
        None => Uri::try_from(req.uri)?,
    };
    let version = if req.protocol == "HTTP/1.0" {
        Version::HTTP_10
    } else {
        Version::HTTP_11
    };

    let mut headers = HeaderMap::new();
    if !req.headers.contains_key(HOST) {
        match req.authority {
            Some(authority) => {
                headers.insert(HOST, HeaderValue::from_bytes(&authority)?);
            }
            None if version == Version::HTTP_11 => {
                headers.insert(HOST, HeaderValue::from_static("localhost"));
            }
            None => {}
        }
    }
    for (name, value) in &req.headers {
        headers.append(name, value.clone());
    }
    if let Some(content_type) = req.content_type
        && !headers.contains_key(CONTENT_TYPE)
    {
        headers.insert(CONTENT_TYPE, HeaderValue::from_bytes(&content_type)?);
    }

    let mut request = http::Request::new(Full::new(Bytes::from(body.into_inner())));
    *request.method_mut() = Method::from_bytes(req.method.as_bytes())?;
    *request.uri_mut() = uri;
    *request.version_mut() = version;
    *request.headers_mut() = headers;
    Ok(request)
}

async fn exchange(
    stream: TcpStream,
    request: http::Request<Full<Bytes>>,
    head_only: bool,
    tx: &mpsc::Sender<Frame>,
) {
    let Ok((mut sender, conn)) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    else {
        return;
    };
    let reply = async move {
        if let Ok(response) = sender.send_request(request).await {
            forward(response, head_only, tx).await;
        }
    };
    // The connection ends once `sender` is gone with the reply, so both futures finish.
    let _ = tokio::join!(conn, reply);
}

async fn forward(
    response: http::Response<hyper::body::Incoming>,
    head_only: bool,
    tx: &mpsc::Sender<Frame>,
) {
    let (parts, mut body) = response.into_parts();
    let content_length = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse().ok());
    let bodiless = head_only
        || matches!(
            parts.status,
            StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED
        );
    let head = Frame::Head {
        head: ResponseHead {
            status: parts.status.as_u16(),
            headers: parts.headers,
        },
        content_length,
        bodiless,
    };
    if tx.send(head).await.is_err() {
        return;
    }

    let mut trailers = HeaderMap::new();
    let truncated = loop {
        match body.frame().await {
            None => break false,
            Some(Err(_)) => break true,
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => {
                    if tx.send(Frame::Chunk(data)).await.is_err() {
                        return;
                    }
                }
                Err(frame) => {
                    if let Ok(fields) = frame.into_trailers() {
                        trailers = fields;
                    }
                }
            },
        }
    };
    let _ = tx
        .send(Frame::End {
            trailers,
            truncated,
        })
        .await;
}
