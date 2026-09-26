use http::{HeaderMap, HeaderName, HeaderValue};
use rapira_sapi::{Addr, Frame, Request};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub mod grpc;
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

/// Build a minimal `GET` request for `uri`.
pub fn req(uri: &str) -> Request {
    Request {
        https: false,
        method: "GET".into(),
        uri: uri.into(),
        target: None,
        authority: None,
        protocol: "HTTP/1.1".into(),
        remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server_name: "localhost".into(),
        server_port: 8080,
        headers: HeaderMap::new(),
        content_type: None,
        content_length: 0,
        body: rapira_sapi::types::Body::Raw(std::io::Cursor::new(Vec::new())),
        received_at: None,
        tls: None,
    }
}

/// The FileDescriptorSet of `fixtures/grpc/echo.proto`.
pub fn echo_descriptor_set() -> PathBuf {
    fixture("grpc/echo.binpb")
}

/// A response stream collected to its `End` (or to the producer dying).
#[derive(Debug, Default)]
pub struct Resp {
    pub interim: Vec<rapira_sapi::ResponseHead>,
    pub head: Option<rapira_sapi::ResponseHead>,
    pub content_length: Option<u64>,
    pub bodiless: bool,
    pub body: Vec<u8>,
    pub trailers: HeaderMap,
    pub truncated: bool,
    /// An `End` frame arrived; false = the producer died first.
    pub ended: bool,
    /// Head frames seen; `head` keeps only the last, so a duplicate would otherwise be invisible.
    pub heads: u32,
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
            Frame::Interim(h) => self.interim.push(h),
            Frame::Head {
                head,
                content_length,
                bodiless,
                ..
            } => {
                self.heads += 1;
                self.head = Some(head);
                self.content_length = content_length;
                self.bodiless = bodiless;
            }
            Frame::Chunk(b) => self.body.extend_from_slice(&b),
            Frame::File { file, offset, len } => match read_slice(&file, offset, len) {
                Ok(bytes) => self.body.extend_from_slice(&bytes),
                Err(e) => panic!("reading a File frame: {e}"),
            },
            Frame::End {
                trailers,
                truncated,
            } => {
                self.trailers = trailers;
                self.truncated = truncated;
                self.ended = true;
                return true;
            }
        }
        false
    }
}

fn read_slice(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut out = vec![0u8; usize::try_from(len).unwrap_or(usize::MAX)];
    let mut done = 0usize;
    while done < out.len() {
        let n = file.read_at(&mut out[done..], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    Ok(out)
}

/// Drains `reply` to its `End`; a missing head, a missing `End` or a truncated `End` is an error.
pub async fn collect(reply: mpsc::Receiver<Frame>) -> anyhow::Result<Resp> {
    let r = drain_resp_async(reply).await;
    if r.head.is_none() && !r.ended {
        anyhow::bail!("php worker died mid-response (channel closed without a response)");
    }
    anyhow::ensure!(
        r.ended && !r.truncated,
        "php crashed mid-response; body truncated"
    );
    anyhow::ensure!(r.head.is_some(), "php produced no response head");
    Ok(r)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_sapi::ResponseHead;

    /// A reply that yields `events` and then closes.
    fn reply(events: Vec<Frame>) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for ev in events {
            tx.try_send(ev).unwrap();
        }
        rx
    }

    fn head() -> Frame {
        Frame::Head {
            head: ResponseHead {
                status: 200,
                headers: fields(&[("x-a", "1")]),
            },
            content_length: None,
            bodiless: false,
        }
    }

    fn end(truncated: bool) -> Frame {
        Frame::End {
            trailers: HeaderMap::new(),
            truncated,
        }
    }

    /// Each failed stream maps to its error: no events, no `End`, a truncated `End`, no head.
    #[tokio::test]
    async fn collect_maps_stream_outcomes() {
        let died = collect(reply(Vec::new())).await.unwrap_err();
        assert!(died.to_string().contains("died mid-response"), "{died:#}");

        let cut = collect(reply(vec![head()])).await.unwrap_err();
        assert!(cut.to_string().contains("truncated"), "{cut:#}");

        let cut = collect(reply(vec![head(), end(true)])).await.unwrap_err();
        assert!(cut.to_string().contains("truncated"), "{cut:#}");

        let headless = collect(reply(vec![end(false)])).await.unwrap_err();
        assert!(
            headless.to_string().contains("no response head"),
            "{headless:#}"
        );
    }

    /// Chunks concatenate in order; interim heads are dropped.
    #[tokio::test]
    async fn collect_concatenates_the_stream() {
        let r = collect(reply(vec![
            Frame::Interim(ResponseHead {
                status: 103,
                headers: HeaderMap::new(),
            }),
            head(),
            Frame::Chunk(b"one,"[..].into()),
            Frame::Chunk(b"two"[..].into()),
            end(false),
        ]))
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.head.unwrap().headers, fields(&[("x-a", "1")]));
        assert_eq!(r.body, b"one,two");
    }
}
