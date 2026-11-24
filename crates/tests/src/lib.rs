use crate::wire::{Frame, ResponseHead};
use http::{HeaderMap, HeaderName, HeaderValue};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub mod grpc;
pub mod metrics;
pub mod server_log;
pub mod wire;

/// The absolute path of a PHP fixture in this crate. The path does not depend on the cwd of the test.
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

/// A GET request for `uri` with `Host: localhost`; the body is the raw bytes.
pub fn req(uri: &str) -> http::Request<Vec<u8>> {
    http::Request::builder()
        .uri(uri)
        .header(http::header::HOST, "localhost")
        .body(Vec::new())
        .expect("a valid test request")
}

/// Calls `check` every 50 ms until it returns Some or `within` has passed. Returns None when `within` has passed.
pub fn poll<T>(within: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + within;
    loop {
        if let Some(v) = check() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The FileDescriptorSet of `fixtures/grpc/echo.proto`.
pub fn echo_descriptor_set() -> PathBuf {
    fixture("grpc/echo.binpb")
}

/// A response stream collected until its `End`, or until the producer stops.
#[derive(Debug, Default)]
pub struct Resp {
    pub head: Option<ResponseHead>,
    pub content_length: Option<u64>,
    pub body: Vec<u8>,
    pub truncated: bool,
    /// An `End` frame arrived. False when the producer stopped first.
    pub ended: bool,
}

impl Resp {
    /// The status, or 0 when the producer stopped before a head or the response never recorded one.
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

    /// Folds one frame in. Returns true when the stream is over.
    pub fn fold(&mut self, frame: Frame) -> bool {
        match frame {
            Frame::Head {
                head,
                content_length,
            } => {
                self.head = Some(head);
                self.content_length = content_length;
            }
            Frame::Chunk(b) => self.body.extend_from_slice(&b),
            Frame::End { truncated } => {
                self.truncated = truncated;
                self.ended = true;
                return true;
            }
        }
        false
    }
}

/// Reads one response until its `End`, until the producer stops, or until `deadline`. Returns None at the deadline, and `Resp::default()` when the producer stops with no frames.
pub fn drain_resp_deadline(
    rx: &mut mpsc::Receiver<Frame>,
    deadline: std::time::Instant,
) -> Option<Resp> {
    let mut resp = Resp::default();
    loop {
        match rx.try_recv() {
            Ok(frame) => {
                if resp.fold(frame) {
                    return Some(resp);
                }
            }
            Err(mpsc::error::TryRecvError::Disconnected) => return Some(resp),
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

/// The status code and the body of one response.
#[derive(Debug, PartialEq)]
pub struct StatusBody<B> {
    pub status: u16,
    pub body: B,
}

/// Reads the stream to its end and returns the status and the body as text. The status is 0 when no head arrived.
pub fn drain(rx: mpsc::Receiver<Frame>) -> StatusBody<String> {
    let r = drain_resp(rx);
    StatusBody {
        status: r.status(),
        body: r.body_string(),
    }
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

pub async fn drain_async(rx: mpsc::Receiver<Frame>) -> StatusBody<String> {
    let r = drain_resp_async(rx).await;
    StatusBody {
        status: r.status(),
        body: r.body_string(),
    }
}

#[cfg(test)]
mod tests {
    use hyper::body::Bytes;

    use super::*;

    #[derive(Debug, PartialEq)]
    struct Got {
        status: u16,
        body: String,
        ended: bool,
    }

    fn head() -> Frame {
        Frame::Head {
            head: ResponseHead {
                status: 200,
                headers: HeaderMap::new(),
            },
            content_length: None,
        }
    }

    #[test]
    fn drain_resp_deadline_bounds_the_whole_response() {
        const WITHIN: Duration = Duration::from_millis(100);
        const OPEN: Duration = Duration::from_secs(1);
        struct Case {
            name: &'static str,
            /// The frames in the channel when the read starts.
            sent: Vec<Frame>,
            /// The producer is open when the read starts. If false, it stopped before the read.
            open: bool,
            want: Option<Got>,
        }
        let cases = [
            Case {
                name: "the response ends before the deadline",
                sent: vec![
                    head(),
                    Frame::Chunk(Bytes::from_static(b"ok")),
                    Frame::End { truncated: false },
                ],
                open: false,
                want: Some(Got {
                    status: 200,
                    body: "ok".to_owned(),
                    ended: true,
                }),
            },
            Case {
                name: "the body does not end before the deadline",
                sent: vec![head(), Frame::Chunk(Bytes::from_static(b"pa"))],
                open: true,
                want: None,
            },
            Case {
                name: "no frame arrives before the deadline",
                sent: vec![],
                open: true,
                want: None,
            },
            Case {
                name: "the producer stops with no frame",
                sent: vec![],
                open: false,
                want: Some(Got {
                    status: 0,
                    body: String::new(),
                    ended: false,
                }),
            },
            Case {
                name: "the producer stops in the body",
                sent: vec![head(), Frame::Chunk(Bytes::from_static(b"pa"))],
                open: false,
                want: Some(Got {
                    status: 200,
                    body: "pa".to_owned(),
                    ended: false,
                }),
            },
        ];
        for case in cases {
            let (tx, mut rx) = mpsc::channel(4);
            for frame in case.sent {
                assert!(tx.try_send(frame).is_ok(), "{}", case.name);
            }
            // An open producer stops 1 s after the read starts, later than the deadline.
            let producer = if case.open {
                Some(std::thread::spawn(move || {
                    std::thread::sleep(OPEN);
                    drop(tx);
                }))
            } else {
                drop(tx);
                None
            };
            let resp = drain_resp_deadline(&mut rx, Instant::now() + WITHIN);
            if let Some(producer) = producer {
                producer.join().expect("the producer thread ends");
            }
            let got = resp.map(|r| Got {
                status: r.status(),
                body: r.body_string(),
                ended: r.ended,
            });
            assert_eq!(got, case.want, "{}", case.name);
        }
    }
}
