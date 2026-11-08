use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_LENGTH, COOKIE, HeaderMap, HeaderName};
use std::ffi::CString;
use std::path::PathBuf;
use tokio::sync::mpsc::{Sender, error::TrySendError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Inet(std::net::SocketAddr),
    /// None is an unnamed endpoint, the usual case for a unix peer.
    Unix(Option<PathBuf>),
}

/// A well-formed response stream is `Head? (Chunk|File)* End?`; a channel closing without `End` means the producer died.
pub enum Frame {
    Head {
        head: ResponseHead,
        /// Some: the framing the consumer must apply; None: the consumer chooses (chunked on HTTP/1.1).
        content_length: Option<u64>,
        /// 204, 304, 101 or a HEAD request: no body bytes and no framing fields on the wire.
        bodiless: bool,
    },
    Chunk(Bytes),
    File {
        file: std::fs::File,
        offset: u64,
        /// Never zero: a producer does not emit an empty slice.
        len: u64,
    },
    End {
        trailers: HeaderMap,
        truncated: bool,
    },
}

/// `unlink` is the one remover: seal calls it at finalize; Drop removes a file that a bailout (fatal or timeout) leaves behind.
pub struct SpooledFile {
    pub path: PathBuf,
}

impl SpooledFile {
    /// Takes the path, so a later Drop is a no-op and a recycled file name is never removed twice.
    pub fn unlink(&mut self) {
        let path = std::mem::take(&mut self.path);
        if path.as_os_str().is_empty() {
            return;
        }
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(target: "rapira", "removing spool file {}: {e}", path.display());
        }
    }
}

impl Drop for SpooledFile {
    fn drop(&mut self) {
        self.unlink();
    }
}

pub struct FormField {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
    pub headers: Vec<(String, Vec<u8>)>,
}

pub struct UploadedFile {
    pub name: Vec<u8>,
    pub client_filename: Vec<u8>,
    /// content-type verbatim; an empty or OWS-only value maps to None upstream.
    pub client_media_type: Option<Vec<u8>>,
    pub headers: Vec<(String, Vec<u8>)>,
    pub file: SpooledFile,
    /// Bytes written to disk; a 64-bit zend_long is assumed at the FFI edge.
    pub size: u64,
}

pub struct MultipartBody {
    pub fields: Vec<FormField>,
    pub files: Vec<UploadedFile>,
}

/// The raw body as the transport delivered it: one part per received data frame, empty frames left out. Each consumer makes its one copy from the parts.
#[derive(Default)]
pub struct Chunks {
    parts: Vec<Bytes>,
    /// The sum of the part lengths.
    len: usize,
    /// The part where the next read starts.
    part: usize,
    /// The read position inside that part.
    offset: usize,
}

impl Chunks {
    pub fn new(mut parts: Vec<Bytes>) -> Self {
        parts.retain(|p| !p.is_empty());
        let len = parts.iter().map(Bytes::len).sum();
        Self {
            parts,
            len,
            part: 0,
            offset: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn parts(&self) -> &[Bytes] {
        &self.parts
    }
}

/// read_post reads through this. A read fills the buffer across the part seams: sapi_read_post_block takes a short read as the end of the body.
/// https://github.com/php/php-src/blob/php-8.5.11/main/SAPI.c#L232-L252
impl std::io::Read for Chunks {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = 0;
        while n < buf.len()
            && let Some(part) = self.parts.get(self.part)
        {
            let take = (part.len() - self.offset).min(buf.len() - n);
            buf[n..n + take].copy_from_slice(&part[self.offset..self.offset + take]);
            n += take;
            self.offset += take;
            if self.offset == part.len() {
                self.part += 1;
                self.offset = 0;
            }
        }
        Ok(n)
    }
}

pub enum Body {
    Raw(Chunks),
    Multipart(MultipartBody),
}

pub struct Request {
    pub method: String,
    pub uri: String,
    /// Request-target bytes; a plugin can reconstruct them from its parsed URI. None falls back to `uri`'s bytes.
    pub target: Option<Vec<u8>>,
    /// Byte-for-byte as the client named it; None = the client named none.
    pub authority: Option<Vec<u8>>,
    pub https: bool,
    /// Wire/CGI spelling ("HTTP/2.0"); mapped to the contract spelling at the exchange view.
    pub protocol: &'static str,
    pub remote: Addr,
    pub server: Addr,
    /// Configured CGI SERVER_NAME/SERVER_PORT, also the $uri synthesis fallback.
    pub server_name: &'static str,
    pub server_port: u16,
    pub headers: HeaderMap,
    /// First content-type field line, raw bytes.
    pub content_type: Option<Vec<u8>>,
    /// Wire byte count, never re-derived from parsed parts; -1 if unknown.
    pub content_length: i64,
    pub body: Body,
    /// Unix seconds.
    pub received_at: f64,
}

#[derive(Debug)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: HeaderMap,
}

/// CGI `Status` pseudo-field: php-src passes it as a plain header line.
/// https://www.rfc-editor.org/rfc/rfc3875#section-6.3.3
static STATUS: HeaderName = HeaderName::from_static("status");

fn status_field_code(value: &[u8]) -> Option<u16> {
    let digits: &[u8] = value
        .split(|b| b.is_ascii_whitespace())
        .find(|token| !token.is_empty())?;
    let code: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (100..=599).contains(&code).then_some(code)
}

/// Owns the $_SERVER-facing strings so `register_server_variables` only borrows: that frame may not hold owned Rust values, a bailout longjmp over pending drops is UB.
pub struct ReqC {
    pub method: CString,
    pub query: CString,
    pub uri: CString,
    pub script: CString,
    pub ctype: Option<CString>,
    pub cookie: Option<CString>,
    pub authorization: Option<CString>,
    pub remote_addr: String,
    pub remote_port: String,
    pub server_port: String,
}

/// The REMOTE_ADDR and REMOTE_PORT values of one peer.
struct CgiAddr {
    addr: String,
    port: String,
}

fn cgi_addr_strings(addr: &Addr) -> CgiAddr {
    match addr {
        Addr::Inet(sa) => CgiAddr {
            addr: sa.ip().to_string(),
            port: sa.port().to_string(),
        },
        // REMOTE_ADDR must hold a hostnumber (RFC 3875 §4.1.8); loopback stands in for a unix peer.
        // https://www.rfc-editor.org/rfc/rfc3875#section-4.1.8
        Addr::Unix(_) => CgiAddr {
            addr: "127.0.0.1".to_owned(),
            port: "0".to_owned(),
        },
    }
}

/// A NUL byte cannot cross the CGI boundary; the value degrades to empty.
fn cgi_cstring(field: &str, bytes: &[u8]) -> CString {
    CString::new(bytes).unwrap_or_else(|_| {
        tracing::warn!(target: "rapira", "{field} carries a NUL byte; registered empty");
        CString::default()
    })
}

impl ReqC {
    /// Cookie repeats rejoin on "; ", the cookie-string form php-src's parser expects.
    pub fn build(r: &Request) -> Self {
        let mut joined = Vec::new();
        let cookie: Option<CString> =
            crate::callbacks::joined_field(&r.headers, &COOKIE, &mut joined)
                .map(|v| cgi_cstring("Cookie", v));

        let authorization: Option<CString> = r
            .headers
            .get(AUTHORIZATION)
            .map(|v| cgi_cstring("Authorization", v.as_bytes()));

        let CgiAddr {
            addr: remote_addr,
            port: remote_port,
        } = cgi_addr_strings(&r.remote);

        Self {
            method: cgi_cstring("REQUEST_METHOD", r.method.as_bytes()),
            query: cgi_cstring(
                "QUERY_STRING",
                r.uri.split_once('?').map_or("", |(_, q)| q).as_bytes(),
            ),
            uri: cgi_cstring("REQUEST_URI", r.uri.as_bytes()),
            script: cgi_cstring(
                "SCRIPT_FILENAME",
                crate::context::script().filename.as_bytes(),
            ),
            cookie,
            authorization,
            ctype: r
                .content_type
                .as_deref()
                .map(|s| cgi_cstring("CONTENT_TYPE", s)),
            remote_addr,
            remote_port,
            server_port: r.server_port.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    NotSent,
    HeadSent,
    /// Body output began during the handler, not the teardown flush.
    BodyStreamed,
}

pub struct Context {
    pub(crate) req: Request,
    pub(crate) c: ReqC,
    pub(crate) sender: Option<Sender<Frame>>,
    pub(crate) head: Option<ResponseHead>,
    pub(crate) body: Vec<u8>,
    pub(crate) stream: StreamState,
    /// The teardown flush runs: its output does not count as a streamed body.
    pub(crate) tearing_down: bool,
    /// The cycle's verdict before the C shutdown, read by the seal inside it.
    pub(crate) errored: bool,
}

impl Context {
    /// Builds the CGI view here: the http plugin calls it on the transport thread.
    pub fn new(req: Request, sender: Sender<Frame>) -> Self {
        let c = ReqC::build(&req);
        Self {
            req,
            c,
            sender: Some(sender),
            head: None,
            body: Vec::new(),
            stream: StreamState::NotSent,
            tearing_down: false,
            errored: false,
        }
    }

    /// The seal of a cycle: a failure gets a 500 head when none was sent, and a body cut mid-stream gets no content-length.
    pub(crate) fn end(&mut self, errored: bool) {
        let truncated = errored && self.stream == StreamState::BodyStreamed;
        if errored && self.stream == StreamState::NotSent {
            self.commit_head(500, HeaderMap::new());
        }
        self.finish(truncated);
    }

    /// The seal of a unit the worker does not serve: the status head, then End.
    pub fn refuse(&mut self, status: u16) {
        if self.stream == StreamState::NotSent {
            self.commit_head(status, HeaderMap::new());
        }
        self.finish(false);
    }

    pub(crate) fn commit_head(&mut self, mut status: u16, mut headers: HeaderMap) {
        for value in headers.get_all(&STATUS) {
            match status_field_code(value.as_bytes()) {
                Some(code) => status = code,
                None => tracing::warn!(
                    target: "php",
                    "ignored malformed Status field {:?}; status stays {status}",
                    String::from_utf8_lossy(value.as_bytes())
                ),
            }
        }
        headers.remove(&STATUS);
        self.head = Some(ResponseHead { status, headers });
        self.stream = StreamState::HeadSent;
    }

    /// Seals the buffered response as Head+Chunk+End; a truncated body gets no content-length, so the client can detect the cut.
    pub(crate) fn finish(&mut self, truncated: bool) {
        let Some(tx) = self.sender.take() else {
            return;
        };
        let body = std::mem::take(&mut self.body);
        if let Some(head) = self.head.take() {
            let no_content = matches!(head.status, 204 | 304) || (100..200).contains(&head.status);
            let bodiless = no_content || self.req.method == "HEAD";
            let content_length = if no_content {
                None
            } else if bodiless {
                // A HEAD response keeps the length that the app declared for the GET response.
                head.headers
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok()?.parse().ok())
            } else {
                (!truncated).then_some(body.len() as u64)
            };
            send(
                &tx,
                Frame::Head {
                    head,
                    content_length,
                    bodiless,
                },
            );
            if !body.is_empty() {
                send(&tx, Frame::Chunk(body.into()));
            }
        }
        send(
            &tx,
            Frame::End {
                trailers: HeaderMap::new(),
                truncated,
            },
        );
    }
}

/// Blocks only on a full channel: `blocking_send` runs a `block_on` on every call.
fn send(tx: &Sender<Frame>, frame: Frame) {
    if let Err(TrySendError::Full(frame)) = tx.try_send(frame) {
        let _ = tx.blocking_send(frame);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn status_field_code_takes_the_leading_integer() {
        assert_eq!(status_field_code(b"404"), Some(404));
        assert_eq!(status_field_code(b"404 Not Found"), Some(404));
        assert_eq!(status_field_code(b"  503   "), Some(503));
    }

    /// An unparseable or out-of-range value leaves the status alone; the caller drops the field either way.
    #[test]
    fn status_field_code_rejects_implausible_values() {
        assert_eq!(status_field_code(b""), None);
        assert_eq!(status_field_code(b"Not Found"), None);
        assert_eq!(status_field_code(b"99"), None);
        assert_eq!(status_field_code(b"600"), None);
        assert_eq!(status_field_code(b"70000"), None);
    }

    /// A GET with no reply sender; a test sets the sender it needs.
    pub(crate) fn context() -> Context {
        Context {
            req: Request {
                method: String::new(),
                uri: String::new(),
                target: None,
                authority: None,
                https: false,
                protocol: "HTTP/1.1",
                remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
                server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
                server_name: "",
                server_port: 8080,
                headers: HeaderMap::new(),
                content_type: None,
                content_length: -1,
                body: Body::Raw(Chunks::default()),
                received_at: 0.0,
            },
            c: ReqC {
                method: CString::default(),
                query: CString::default(),
                uri: CString::default(),
                script: CString::default(),
                ctype: None,
                cookie: None,
                authorization: None,
                remote_addr: String::new(),
                remote_port: String::new(),
                server_port: String::new(),
            },
            sender: None,
            head: None,
            body: Vec::new(),
            stream: StreamState::NotSent,
            tearing_down: false,
            errored: false,
        }
    }

    fn head_of(status: u16, headers: &[(&str, &str)]) -> ResponseHead {
        let mut ctx = context();
        let headers = headers
            .iter()
            .map(|(k, v)| {
                (
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    http::HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect();
        ctx.commit_head(status, headers);
        ctx.head.expect("commit_head records a head")
    }

    /// One line per frame: the fields the plugin acts on.
    pub(crate) fn describe(frame: Frame) -> String {
        match frame {
            Frame::Head {
                head,
                content_length,
                bodiless,
            } => format!(
                "head {} cl={content_length:?} bodiless={bodiless}",
                head.status
            ),
            Frame::Chunk(b) => format!("chunk {}", String::from_utf8_lossy(&b)),
            Frame::File { .. } => "file".into(),
            Frame::End { truncated, .. } => format!("end truncated={truncated}"),
        }
    }

    /// The seal publishes the reply once: a failure gets a 500 head when none was sent, a body cut mid-stream gets no content-length, and a refusal gets its status head.
    #[test]
    fn the_seal_applies_the_verdict_once() {
        struct Case {
            name: &'static str,
            method: &'static str,
            /// The head the cycle committed before the seal.
            head: Option<u16>,
            body: &'static str,
            stream: StreamState,
            seal: fn(&mut Context),
            want: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "a buffered reply ends as head, chunk and end",
                method: "GET",
                head: Some(200),
                body: "ok",
                stream: StreamState::HeadSent,
                seal: |c| c.end(false),
                want: &[
                    "head 200 cl=Some(2) bodiless=false",
                    "chunk ok",
                    "end truncated=false",
                ],
            },
            Case {
                name: "a failure before any head sends a 500",
                method: "GET",
                head: None,
                body: "",
                stream: StreamState::NotSent,
                seal: |c| c.end(true),
                want: &["head 500 cl=Some(0) bodiless=false", "end truncated=false"],
            },
            Case {
                name: "a failure after the head keeps the head and the buffered body",
                method: "GET",
                head: Some(200),
                body: "ok",
                stream: StreamState::HeadSent,
                seal: |c| c.end(true),
                want: &[
                    "head 200 cl=Some(2) bodiless=false",
                    "chunk ok",
                    "end truncated=false",
                ],
            },
            Case {
                name: "a failure after a streamed body ends truncated",
                method: "GET",
                head: None,
                body: "",
                stream: StreamState::BodyStreamed,
                seal: |c| c.end(true),
                want: &["end truncated=true"],
            },
            Case {
                name: "a completed streamed body ends whole",
                method: "GET",
                head: None,
                body: "",
                stream: StreamState::BodyStreamed,
                seal: |c| c.end(false),
                want: &["end truncated=false"],
            },
            Case {
                name: "a second seal sends nothing",
                method: "GET",
                head: Some(200),
                body: "ok",
                stream: StreamState::HeadSent,
                seal: |c| {
                    c.end(false);
                    c.end(true);
                },
                want: &[
                    "head 200 cl=Some(2) bodiless=false",
                    "chunk ok",
                    "end truncated=false",
                ],
            },
            Case {
                name: "a refusal sends the status head and end",
                method: "GET",
                head: None,
                body: "",
                stream: StreamState::NotSent,
                seal: |c| c.refuse(503),
                want: &["head 503 cl=Some(0) bodiless=false", "end truncated=false"],
            },
            Case {
                name: "a refused HEAD request is bodiless",
                method: "HEAD",
                head: None,
                body: "",
                stream: StreamState::NotSent,
                seal: |c| c.refuse(503),
                want: &["head 503 cl=None bodiless=true", "end truncated=false"],
            },
        ];
        for case in cases {
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let mut ctx = context();
            ctx.req.method = case.method.into();
            ctx.sender = Some(tx);
            if let Some(status) = case.head {
                ctx.commit_head(status, HeaderMap::new());
            }
            ctx.body = case.body.into();
            ctx.stream = case.stream;
            (case.seal)(&mut ctx);
            let mut got = Vec::new();
            while let Ok(frame) = rx.try_recv() {
                got.push(describe(frame));
            }
            assert_eq!(got, case.want, "{}", case.name);
        }
    }

    /// The `Status:` field sets the code, never reaches the client, and matches case-insensitively since php-src passes the script's own spelling.
    #[test]
    fn commit_head_consumes_the_status_field() {
        let head = head_of(200, &[("status", "404 Not Found"), ("X-Keep", "kept")]);
        assert_eq!(head.status, 404);
        assert_eq!(head.headers.len(), 1);
        assert!(head.headers.contains_key("x-keep"));
    }

    /// An unparseable `Status:` is still consumed (forwarding it would put a literal `Status:` field on the wire) without inventing a code.
    #[test]
    fn commit_head_drops_an_unparseable_status_without_changing_the_code() {
        let head = head_of(201, &[("Status", "NotFound")]);
        assert_eq!(head.status, 201);
        assert!(head.headers.is_empty());
    }

    /// RFC 3875 §6.3.3 makes `Status` the script's own result code, so it overrides the code php-src already recorded.
    #[test]
    fn commit_head_lets_the_status_field_override_the_recorded_code() {
        assert_eq!(head_of(500, &[("Status", "404")]).status, 404);
    }

    /// A read fills the buffer across the part seams and is short only at the end of the body.
    #[test]
    fn chunks_read_in_order_across_the_parts() {
        use std::io::Read;
        struct Case {
            name: &'static str,
            parts: &'static [&'static [u8]],
            /// The buffer length of each read, in order.
            reads: &'static [usize],
            /// The bytes that each read gives.
            want: &'static [&'static [u8]],
        }
        let cases = [
            Case {
                name: "a read crosses the seam between two parts",
                parts: &[b"abc", b"def"],
                reads: &[4, 4, 1],
                want: &[b"abcd", b"ef", b""],
            },
            Case {
                name: "a read longer than the rest gives the rest",
                parts: &[b"abc", b"def"],
                reads: &[2, 10, 4],
                want: &[b"ab", b"cdef", b""],
            },
            Case {
                name: "a zero-length read in the middle moves nothing",
                parts: &[b"abc", b"def"],
                reads: &[2, 0, 4],
                want: &[b"ab", b"", b"cdef"],
            },
            Case {
                name: "an empty part changes nothing",
                parts: &[b"abc", b"", b"def"],
                reads: &[4, 4],
                want: &[b"abcd", b"ef"],
            },
            Case {
                name: "an empty body reads nothing",
                parts: &[],
                reads: &[4],
                want: &[b""],
            },
        ];
        for case in cases {
            let mut chunks =
                Chunks::new(case.parts.iter().map(|p| Bytes::from_static(p)).collect());
            let mut got: Vec<Vec<u8>> = Vec::new();
            for &n in case.reads {
                let mut buf = vec![0; n];
                let read = chunks.read(&mut buf).unwrap();
                got.push(buf[..read].to_vec());
            }
            assert_eq!(got, case.want, "{}", case.name);
        }
    }
}
