//! An HTTP/1.1 client that sends an `http::Request` to a server and reads the response back as `Frame`s.

use std::net::SocketAddr;
use std::sync::OnceLock;

use http::header::CONTENT_LENGTH;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use rapira_sapi::{Frame, types::ResponseHead};
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
pub fn submit(
    addr: SocketAddr,
    req: http::Request<Vec<u8>>,
) -> anyhow::Result<mpsc::Receiver<Frame>> {
    runtime().block_on(submit_async(addr, req))
}

/// Sends `req` to `addr` on a new connection and returns once the connection is open.
/// The receiver gets `Head`, one `Chunk` per body frame, and `End`; a body error is `End { truncated: true }`, and no frame means that no response head arrived.
/// Dropping the receiver closes the connection at once.
pub async fn submit_async(
    addr: SocketAddr,
    req: http::Request<Vec<u8>>,
) -> anyhow::Result<mpsc::Receiver<Frame>> {
    let request = req.map(|body| Full::new(Bytes::from(body)));
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
