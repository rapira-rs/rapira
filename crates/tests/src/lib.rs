use http::{HeaderMap, HeaderName, HeaderValue};
use rapira_sapi::Frame;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub mod grpc;
pub mod metrics;
pub mod server_log;
pub mod wire;

/// Absolute path to a PHP fixture shipped with this crate (robust to the test's cwd).
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

pub type Fields = &'static [(&'static str, &'static str)];

/// A header map from static (name, value) lines.
pub fn fields(lines: Fields) -> HeaderMap {
    lines
        .iter()
        .map(|&(k, v)| (HeaderName::from_static(k), HeaderValue::from_static(v)))
        .collect()
}

/// Panics when RAPIRA_REQUIRE_EXTS names an extension this fixture covers: a skip where CI installs the extension is a broken install.
pub fn assert_skip_allowed(fixture: &str) {
    let Ok(required) = std::env::var("RAPIRA_REQUIRE_EXTS") else {
        return;
    };
    for ext in required.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        assert!(
            !fixture.contains(ext),
            "{fixture} skipped, but RAPIRA_REQUIRE_EXTS demands {ext}"
        );
    }
}

/// A GET request for `uri` with `Host: localhost`; the body is the raw bytes.
pub fn req(uri: &str) -> http::Request<Vec<u8>> {
    http::Request::builder()
        .uri(uri)
        .header(http::header::HOST, "localhost")
        .body(Vec::new())
        .expect("a valid test request")
}

/// The FileDescriptorSet of `fixtures/grpc/echo.proto`.
pub fn echo_descriptor_set() -> PathBuf {
    fixture("grpc/echo.binpb")
}

/// A response stream collected to its `End` (or to the producer dying).
#[derive(Debug, Default)]
pub struct Resp {
    pub head: Option<rapira_sapi::types::ResponseHead>,
    pub content_length: Option<u64>,
    pub body: Vec<u8>,
    pub trailers: HeaderMap,
    pub truncated: bool,
    /// An `End` frame arrived; false = the producer died first.
    pub ended: bool,
}

impl Resp {
    /// 0 = no head (producer died, or the response never recorded one).
    pub fn status(&self) -> u16 {
        self.head.as_ref().map_or(0, |h| h.status)
    }

    pub fn header(&self, name: &str) -> Option<String> {
        let value = self.head.as_ref()?.headers.get(name)?;
        Some(String::from_utf8_lossy(value.as_bytes()).into_owned())
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Fold one frame in; true when the stream is over.
    fn fold(&mut self, frame: Frame) -> bool {
        match frame {
            Frame::Head {
                head,
                content_length,
                ..
            } => {
                self.head = Some(head);
                self.content_length = content_length;
            }
            Frame::Chunk(b) => self.body.extend_from_slice(&b),
            Frame::End {
                trailers,
                truncated,
            } => {
                self.trailers = trailers;
                self.truncated = truncated;
                self.ended = true;
                return true;
            }
            Frame::Interim(_) | Frame::File { .. } => {
                unreachable!("the wire client sends only Head, Chunk and End")
            }
        }
        false
    }
}

/// Poll for a first frame until `deadline`: None = nothing arrived in time, a producer that died with no frames yields `Resp::default()`.
pub fn drain_resp_deadline(
    rx: &mut mpsc::Receiver<Frame>,
    deadline: std::time::Instant,
) -> Option<Resp> {
    loop {
        match rx.try_recv() {
            Ok(frame) => {
                let mut resp = Resp::default();
                if !resp.fold(frame) {
                    while let Some(f) = rx.blocking_recv() {
                        if resp.fold(f) {
                            break;
                        }
                    }
                }
                return Some(resp);
            }
            Err(mpsc::error::TryRecvError::Disconnected) => return Some(Resp::default()),
            Err(mpsc::error::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

pub fn drain_resp(mut rx: mpsc::Receiver<Frame>) -> Resp {
    let mut resp = Resp::default();
    while let Some(frame) = rx.blocking_recv() {
        if resp.fold(frame) {
            break;
        }
    }
    resp
}

/// Drain to `(status, body)`; status is 0 when no head was produced.
pub fn drain(rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let r = drain_resp(rx);
    (r.status(), r.body_string())
}

pub async fn drain_resp_async(mut rx: mpsc::Receiver<Frame>) -> Resp {
    let mut resp = Resp::default();
    while let Some(frame) = rx.recv().await {
        if resp.fold(frame) {
            break;
        }
    }
    resp
}

pub async fn drain_async(rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let r = drain_resp_async(rx).await;
    (r.status(), r.body_string())
}
