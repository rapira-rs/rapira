use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderName};
use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::Arc;

pub type RequestHeaders = http::HeaderMap;
pub type ResponseHeaders = http::HeaderMap;

/// CGI response head passed from classic and worker modes to [`CgiResponder`].
#[derive(Debug)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: ResponseHeaders,
}

impl ResponseHead {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: ResponseHeaders::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Inet(std::net::SocketAddr),
    /// None is an unnamed endpoint, the usual case for a unix peer.
    Unix(Option<PathBuf>),
}

/// Direct response sink for classic and worker CGI modes.
pub trait CgiResponder: Send + 'static {
    fn is_cancelled(&self) -> bool;

    fn respond(self: Box<Self>, head: ResponseHead, body: Vec<u8>, truncated: bool);
}

/// Inline single-frame storage avoids a `Vec<Bytes>` heap allocation on single-chunk POST bodies.
#[derive(Default)]
enum ChunkParts {
    #[default]
    Empty,
    Single([Bytes; 1]),
    Many(Vec<Bytes>),
}

impl ChunkParts {
    #[inline]
    fn as_slice(&self) -> &[Bytes] {
        match self {
            Self::Empty => &[],
            Self::Single(one) => one.as_slice(),
            Self::Many(many) => many.as_slice(),
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    fn take_vec(&mut self) -> Vec<Bytes> {
        match std::mem::take(self) {
            Self::Empty => Vec::new(),
            Self::Single([one]) => vec![one],
            Self::Many(many) => many,
        }
    }
}

/// The raw body as the transport delivered it. It has one part per received data frame, and it leaves out empty frames. Each consumer makes its one copy from the parts.
#[derive(Default)]
pub struct Chunks {
    parts: ChunkParts,
    /// Shared read-only view created lazily when PHP opens the zero-copy body stream, while keeping `Read` (`read_post`) intact for `request_parse_body()`.
    shared: Option<Arc<BodyData>>,
    /// The sum of the part lengths.
    len: usize,
    /// The part where the next read starts.
    part: usize,
    /// The read position inside that part.
    offset: usize,
}

impl Chunks {
    pub fn single(bytes: Bytes) -> Self {
        if bytes.is_empty() {
            return Self::default();
        }
        let len = bytes.len();
        Self {
            parts: ChunkParts::Single([bytes]),
            shared: None,
            len,
            part: 0,
            offset: 0,
        }
    }

    pub fn new(mut parts: Vec<Bytes>) -> Self {
        parts.retain(|p| !p.is_empty());
        let len = parts.iter().map(Bytes::len).sum();
        let storage = match parts.len() {
            0 => ChunkParts::Empty,
            1 => ChunkParts::Single([parts.into_iter().next().unwrap()]),
            _ => ChunkParts::Many(parts),
        };
        Self {
            parts: storage,
            shared: None,
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
        match &self.shared {
            Some(data) => &data.parts,
            None => self.parts.as_slice(),
        }
    }

    /// Shares the raw byte chunks for PHP's zero-copy body stream while keeping `len()`, `parts()`, and `Read` (`read_post`) working.
    pub(crate) fn body_data(&mut self) -> Option<Arc<BodyData>> {
        if let Some(data) = &self.shared {
            return Some(Arc::clone(data));
        }
        if self.parts.is_empty() {
            return None;
        }
        let parts = self.parts.take_vec();
        let data = Arc::new(BodyData {
            parts,
            len: self.len,
        });
        self.shared = Some(Arc::clone(&data));
        Some(data)
    }
}

/// Shared immutable request body chunks backing zero-copy PHP `request_body` and `php://input` streams.
pub(crate) struct BodyData {
    parts: Vec<Bytes>,
    len: usize,
}

/// Independent seekable read cursor over shared `BodyData`.
pub(crate) struct BodyCursor {
    data: Arc<BodyData>,
    part: usize,
    offset: usize,
    pos: usize,
}

impl BodyCursor {
    pub(crate) fn new(data: Arc<BodyData>) -> Self {
        Self {
            data,
            part: 0,
            offset: 0,
            pos: 0,
        }
    }

    /// Opens a fresh cursor at byte 0 sharing the same underlying chunks without copying bytes.
    pub(crate) fn clone_at_start(&self) -> Self {
        Self::new(Arc::clone(&self.data))
    }

    pub(crate) fn len(&self) -> usize {
        self.data.len
    }

    /// Copies up to `dst.len()` bytes and reports whether the cursor reached or passed EOF.
    pub(crate) fn read(&mut self, dst: &mut [u8]) -> (usize, bool) {
        let n = read_parts(&self.data.parts, &mut self.part, &mut self.offset, dst);
        self.pos += n;
        (n, self.pos >= self.data.len)
    }

    /// Repositions the cursor following `php_stream_memory_seek` semantics. Returns `None` on invalid whence or negative target position.
    pub(crate) fn seek(&mut self, offset: i64, whence: i32) -> Option<i64> {
        let target: i128 = match whence {
            libc::SEEK_SET => i128::from(offset),
            libc::SEEK_CUR => (self.pos as i128) + i128::from(offset),
            libc::SEEK_END => (self.data.len as i128) + i128::from(offset),
            _ => return None,
        };
        if target < 0 || target > i128::from(i64::MAX) {
            return None;
        }
        let pos = target as usize;
        self.pos = pos;
        if pos == 0 {
            self.part = 0;
            self.offset = 0;
        } else if pos >= self.data.len {
            self.part = self.data.parts.len();
            self.offset = 0;
        } else {
            let mut rem = pos;
            for (idx, part) in self.data.parts.iter().enumerate() {
                if rem < part.len() {
                    self.part = idx;
                    self.offset = rem;
                    break;
                }
                rem -= part.len();
            }
        }
        Some(target as i64)
    }
}

fn read_parts(parts: &[Bytes], part: &mut usize, offset: &mut usize, dst: &mut [u8]) -> usize {
    let mut n = 0;
    while n < dst.len()
        && let Some(p) = parts.get(*part)
    {
        let take = (p.len() - *offset).min(dst.len() - n);
        dst[n..n + take].copy_from_slice(&p[*offset..*offset + take]);
        n += take;
        *offset += take;
        if *offset == p.len() {
            *part += 1;
            *offset = 0;
        }
    }
    n
}

/// read_post reads through this. A read fills the buffer across the part boundaries, because sapi_read_post_block takes a short read as the end of the body.
/// https://github.com/php/php-src/blob/php-8.5.11/main/SAPI.c#L232-L252
impl std::io::Read for Chunks {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let parts = match &self.shared {
            Some(data) => data.parts.as_slice(),
            None => self.parts.as_slice(),
        };
        Ok(read_parts(parts, &mut self.part, &mut self.offset, buf))
    }
}

/// Origin-form or asterisk-form URI kept zero-copy from `http::Uri` when possible.
pub enum RequestUri {
    Static(&'static str),
    Parsed(http::Uri),
    Owned(String),
}

impl RequestUri {
    #[inline]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Static(s) => s,
            Self::Parsed(u) => u.path_and_query().map_or("/", |pq| pq.as_str()),
            Self::Owned(s) => s.as_str(),
        }
    }
}

/// Request authority kept zero-copy from the `Host` header in `Request.headers` on the common HTTP/1.1 path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    None,
    HostHeader,
    Explicit(Bytes),
}

pub struct Request {
    pub method: Cow<'static, str>,
    pub uri: RequestUri,
    /// The request-target bytes. A plugin can rebuild them from its parsed URI. None uses the bytes of `uri`.
    pub target: Option<Vec<u8>>,
    /// The authority byte for byte as the client named it.
    pub authority: Authority,
    pub https: bool,
    /// The wire and CGI spelling, for example "HTTP/2.0". The exchange view maps it to the contract spelling.
    pub protocol: &'static str,
    pub remote: Addr,
    pub server: Addr,
    pub headers: RequestHeaders,
    pub body: Chunks,
    /// Unix seconds.
    pub received_at: f64,
}

impl Request {
    #[inline]
    pub fn authority_bytes(&self) -> Option<&[u8]> {
        match &self.authority {
            Authority::None => None,
            Authority::HostHeader => self.headers.get(HOST).map(|v| v.as_bytes().trim_ascii()),
            Authority::Explicit(b) => Some(b.as_ref()),
        }
    }
}

/// The CGI `Status` pseudo-field. php-src passes it as a plain header line.
/// https://www.rfc-editor.org/rfc/rfc3875#section-6.3.3
static STATUS: HeaderName = HeaderName::from_static("status");

fn status_field_code(value: &[u8]) -> Option<u16> {
    let digits: &[u8] = value
        .split(|b| b.is_ascii_whitespace())
        .find(|token| !token.is_empty())?;
    let code: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (100..=599).contains(&code).then_some(code)
}

/// The CGI input stays valid until the PHP thread unbinds the exchange.
pub(crate) struct CgiRequest {
    pub method: Cow<'static, CStr>,
    pub query: Cow<'static, CStr>,
    pub uri: Cow<'static, CStr>,
    pub path_len: usize,
    pub ctype: Option<CString>,
    pub authorization: Option<CString>,
    pub remote: Addr,
    pub https: bool,
    pub protocol: &'static str,
    pub headers: crate::callbacks::CgiHeaders,
    pub body: Chunks,
}

/// A NUL byte cannot cross the CGI boundary, so the value becomes empty.
pub(crate) fn cgi_cstring(field: &str, bytes: impl Into<Vec<u8>>) -> CString {
    CString::new(bytes).unwrap_or_else(|_| {
        tracing::warn!(target: "rapira", "{field} carries a NUL byte; registered empty");
        CString::default()
    })
}

fn cgi_method(method: Cow<'static, str>) -> Cow<'static, CStr> {
    match method.as_ref() {
        "GET" => Cow::Borrowed(c"GET"),
        "POST" => Cow::Borrowed(c"POST"),
        "PUT" => Cow::Borrowed(c"PUT"),
        "DELETE" => Cow::Borrowed(c"DELETE"),
        "HEAD" => Cow::Borrowed(c"HEAD"),
        "OPTIONS" => Cow::Borrowed(c"OPTIONS"),
        "PATCH" => Cow::Borrowed(c"PATCH"),
        "CONNECT" => Cow::Borrowed(c"CONNECT"),
        "TRACE" => Cow::Borrowed(c"TRACE"),
        _ => Cow::Owned(cgi_cstring(
            "REQUEST_METHOD",
            method.into_owned().into_bytes(),
        )),
    }
}

impl From<Request> for CgiRequest {
    fn from(r: Request) -> Self {
        let uri_str = r.uri.as_str();
        let (path, query) = uri_str.split_once('?').unwrap_or((uri_str, ""));
        let path_len = path.len();
        let query = if query.is_empty() {
            Cow::Borrowed(c"")
        } else {
            Cow::Owned(cgi_cstring("QUERY_STRING", query.as_bytes()))
        };
        let uri = if uri_str == "/" {
            Cow::Borrowed(c"/")
        } else {
            Cow::Owned(cgi_cstring("REQUEST_URI", uri_str.as_bytes()))
        };
        let authorization = r
            .headers
            .get(AUTHORIZATION)
            .map(|v| cgi_cstring("Authorization", v.as_bytes()));
        let ctype = r
            .headers
            .get(CONTENT_TYPE)
            .map(|v| cgi_cstring("CONTENT_TYPE", v.as_bytes()));
        Self {
            method: cgi_method(r.method),
            query,
            path_len: path_len.min(uri.to_bytes().len()),
            uri,
            authorization,
            ctype,
            remote: r.remote,
            https: r.https,
            protocol: r.protocol,
            headers: crate::callbacks::CgiHeaders::new(r.headers),
            body: r.body,
        }
    }
}

pub const MAX_BUFFERED_BODY: usize = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    NotSent,
    HeadSent,
    /// Body output starts during the handler, not during the teardown flush.
    BodyStreamed,
}

/// Encapsulated per-request CGI exchange state for classic and worker modes.
pub struct CgiExchange {
    req: CgiRequest,
    responder: Option<Box<dyn CgiResponder>>,
    head: Option<ResponseHead>,
    body: Vec<u8>,
    stream: StreamState,
    /// True while the teardown flush runs. Its output does not count as a streamed body.
    tearing_down: bool,
    /// The error result of the cycle before the C shutdown. The seal inside the shutdown reads it.
    errored: bool,
}

impl CgiExchange {
    /// Builds the CGI view here, because the http plugin calls it on the IO thread.
    pub fn new(req: Request, responder: Box<dyn CgiResponder>) -> Self {
        Self {
            req: req.into(),
            responder: Some(responder),
            head: None,
            body: Vec::new(),
            stream: StreamState::NotSent,
            tearing_down: false,
            errored: false,
        }
    }

    #[inline]
    pub(crate) fn req(&self) -> &CgiRequest {
        &self.req
    }

    #[inline]
    pub(crate) fn req_mut(&mut self) -> &mut CgiRequest {
        &mut self.req
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.responder.as_ref().is_none_or(|r| r.is_cancelled())
    }

    /// Commits headers once if no head has been sent yet, running `build` only when needed.
    #[inline]
    pub(crate) fn send_headers(&mut self, build: impl FnOnce() -> (u16, ResponseHeaders)) {
        if self.stream == StreamState::NotSent {
            let (status, headers) = build();
            self.commit_head(status, headers);
        }
    }

    /// Buffers response body bytes from PHP `ub_write`, committing an implicit head on first write.
    /// Returns `Err(())` and seals the exchange when the client cancelled or the buffer cap is exceeded.
    pub(crate) fn write_body(
        &mut self,
        data: &[u8],
        fallback_status: impl FnOnce() -> u16,
    ) -> Result<usize, ()> {
        if self.stream == StreamState::NotSent {
            self.commit_head(fallback_status(), ResponseHeaders::new());
        }
        if let Some(responder) = &self.responder {
            if responder.is_cancelled() {
                self.finish(false);
                return Err(());
            }
            if self.body.len().saturating_add(data.len()) > MAX_BUFFERED_BODY {
                tracing::error!(
                    target: "rapira",
                    "response body exceeds the host buffer cap ({} + {} > {MAX_BUFFERED_BODY} bytes); aborting the request",
                    self.body.len(),
                    data.len()
                );
                self.finish(true);
                return Err(());
            }
            if self.body.is_empty() && !data.is_empty() {
                self.body.reserve(data.len().max(4096));
            }
            self.body.extend_from_slice(data);
            if !self.tearing_down {
                self.stream = StreamState::BodyStreamed;
            }
        }
        Ok(data.len())
    }

    pub(crate) fn begin_teardown(&mut self) {
        self.tearing_down = true;
    }

    pub(crate) fn mark_errored(&mut self, errored: bool) {
        self.errored = errored;
    }

    /// Final seal invoked from RSHUTDOWN after shutdown functions and destructors finish.
    pub(crate) fn finish_response(&mut self, fatal_recorded: bool) {
        let errored = self.errored || fatal_recorded;
        self.end(errored);
    }

    /// The seal of a cycle. A failure gets a 500 head when none was sent, and a body cut mid-stream gets no content-length.
    pub(crate) fn end(&mut self, errored: bool) {
        let truncated = errored && self.stream == StreamState::BodyStreamed;
        if errored && self.stream == StreamState::NotSent {
            self.commit_head(500, ResponseHeaders::new());
        }
        self.finish(truncated);
    }

    /// The seal of a unit the worker does not serve: the status head, then End.
    pub fn refuse(&mut self, status: u16) {
        if self.stream == StreamState::NotSent {
            self.commit_head(status, ResponseHeaders::new());
        }
        self.finish(false);
    }

    fn commit_head(&mut self, mut status: u16, mut headers: ResponseHeaders) {
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

    /// Seals the buffered response through the responder; the responder applies transport framing rules.
    fn finish(&mut self, truncated: bool) {
        let body = std::mem::take(&mut self.body);
        let Some(responder) = self.responder.take() else {
            return;
        };
        let head = self.head.take().unwrap_or_else(|| ResponseHead::new(200));
        responder.respond(head, body, truncated);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    struct TestResponder {
        cancelled: bool,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl CgiResponder for TestResponder {
        fn is_cancelled(&self) -> bool {
            self.cancelled
        }

        fn respond(self: Box<Self>, head: ResponseHead, body: Vec<u8>, truncated: bool) {
            self.log.lock().unwrap().push(format!(
                "respond {} truncated={truncated} body={}",
                head.status,
                String::from_utf8_lossy(&body)
            ));
        }
    }

    pub(crate) fn responder(cancelled: bool) -> (Box<dyn CgiResponder>, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            Box::new(TestResponder {
                cancelled,
                log: Arc::clone(&log),
            }),
            log,
        )
    }

    #[test]
    fn status_field_code_takes_the_leading_integer() {
        assert_eq!(status_field_code(b"404"), Some(404));
        assert_eq!(status_field_code(b"404 Not Found"), Some(404));
        assert_eq!(status_field_code(b"  503   "), Some(503));
    }

    /// An unparseable or out-of-range value leaves the status unchanged; the caller drops the field in both cases.
    #[test]
    fn status_field_code_rejects_implausible_values() {
        assert_eq!(status_field_code(b""), None);
        assert_eq!(status_field_code(b"Not Found"), None);
        assert_eq!(status_field_code(b"99"), None);
        assert_eq!(status_field_code(b"600"), None);
        assert_eq!(status_field_code(b"70000"), None);
    }

    pub(crate) fn empty_request() -> Request {
        Request {
            method: Cow::Borrowed(""),
            uri: RequestUri::Static(""),
            target: None,
            authority: Authority::None,
            https: false,
            protocol: "HTTP/1.1",
            remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            headers: RequestHeaders::new(),
            body: Chunks::default(),
            received_at: 0.0,
        }
    }

    fn head_of(status: u16, headers: &[(&str, &str)]) -> ResponseHead {
        let (resp, _log) = responder(false);
        let mut ex = CgiExchange::new(empty_request(), resp);
        let mut hdrs = ResponseHeaders::new();
        for (k, v) in headers {
            hdrs.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        ex.send_headers(|| (status, hdrs));
        ex.head.expect("send_headers records a head")
    }

    /// The seal publishes the reply once: a failure gets a 500 head when none was sent, a body cut mid-stream gets no content-length, and a refusal gets its status head.
    #[test]
    fn the_seal_applies_the_verdict_once() {
        struct Case {
            name: &'static str,
            /// The head the cycle committed before the seal.
            head: Option<u16>,
            body: &'static str,
            teardown_write: bool,
            seal: fn(&mut CgiExchange),
            want: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "a buffered reply ends as one response",
                head: Some(200),
                body: "ok",
                teardown_write: true,
                seal: |c| c.end(false),
                want: &["respond 200 truncated=false body=ok"],
            },
            Case {
                name: "a failure before any head sends a 500",
                head: None,
                body: "",
                teardown_write: false,
                seal: |c| c.end(true),
                want: &["respond 500 truncated=false body="],
            },
            Case {
                name: "a failure after teardown flush keeps the head and buffered body without truncation",
                head: Some(200),
                body: "ok",
                teardown_write: true,
                seal: |c| c.end(true),
                want: &["respond 200 truncated=false body=ok"],
            },
            Case {
                name: "a failure after a streamed body marks truncation",
                head: Some(200),
                body: "part",
                teardown_write: false,
                seal: |c| c.end(true),
                want: &["respond 200 truncated=true body=part"],
            },
            Case {
                name: "a second seal sends nothing",
                head: Some(200),
                body: "ok",
                teardown_write: true,
                seal: |c| {
                    c.end(false);
                    c.end(true);
                },
                want: &["respond 200 truncated=false body=ok"],
            },
            Case {
                name: "a refusal sends the status head",
                head: None,
                body: "",
                teardown_write: false,
                seal: |c| c.refuse(503),
                want: &["respond 503 truncated=false body="],
            },
        ];
        for case in cases {
            let (resp, log) = responder(false);
            let mut ex = CgiExchange::new(empty_request(), resp);
            if let Some(status) = case.head {
                ex.send_headers(|| (status, ResponseHeaders::new()));
            }
            if case.teardown_write {
                ex.begin_teardown();
            }
            if !case.body.is_empty() {
                assert_eq!(
                    ex.write_body(case.body.as_bytes(), || 200),
                    Ok(case.body.len()),
                    "{}",
                    case.name
                );
            }
            (case.seal)(&mut ex);
            assert_eq!(log.lock().unwrap().as_slice(), case.want, "{}", case.name);
        }
    }

    #[test]
    fn write_body_and_send_headers_invariants() {
        struct Case {
            name: &'static str,
            cancelled: bool,
            first_head: Option<u16>,
            second_head: Option<u16>,
            write: &'static [u8],
            fallback_status: u16,
            want_write_ok: bool,
            want_log: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "write_body commits implicit fallback status when no head was sent",
                cancelled: false,
                first_head: None,
                second_head: None,
                write: b"hi",
                fallback_status: 201,
                want_write_ok: true,
                want_log: &["respond 201 truncated=false body=hi"],
            },
            Case {
                name: "second send_headers is ignored after head is committed",
                cancelled: false,
                first_head: Some(202),
                second_head: Some(404),
                write: b"ok",
                fallback_status: 200,
                want_write_ok: true,
                want_log: &["respond 202 truncated=false body=ok"],
            },
            Case {
                name: "write_body on cancelled client returns Err and seals without truncation",
                cancelled: true,
                first_head: Some(200),
                second_head: None,
                write: b"drop",
                fallback_status: 200,
                want_write_ok: false,
                want_log: &["respond 200 truncated=false body="],
            },
        ];
        for case in cases {
            let (resp, log) = responder(case.cancelled);
            let mut ex = CgiExchange::new(empty_request(), resp);
            if let Some(s) = case.first_head {
                ex.send_headers(|| (s, ResponseHeaders::new()));
            }
            if let Some(s) = case.second_head {
                ex.send_headers(|| {
                    assert!(
                        case.first_head.is_none(),
                        "{}: second send_headers must not invoke builder",
                        case.name
                    );
                    (s, ResponseHeaders::new())
                });
            }
            let res = ex.write_body(case.write, || case.fallback_status);
            assert_eq!(res.is_ok(), case.want_write_ok, "{}", case.name);
            ex.end(false);
            assert_eq!(
                log.lock().unwrap().as_slice(),
                case.want_log,
                "{}",
                case.name
            );
        }
    }

    /// The `Status:` field sets the code, never reaches the client, and matches case-insensitively, because php-src passes the spelling of the app.
    #[test]
    fn commit_head_consumes_the_status_field() {
        let head = head_of(200, &[("status", "404 Not Found"), ("X-Keep", "kept")]);
        assert_eq!(head.status, 404);
        assert_eq!(head.headers.len(), 1);
        assert!(head.headers.contains_key(HeaderName::from_static("x-keep")));
    }

    /// The host still removes an unparseable `Status:` and invents no code. A forwarded `Status:` would put a literal `Status:` field on the wire.
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

    /// A read fills the buffer across the part boundaries and is short only at the end of the body.
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

    #[test]
    fn chunks_remain_readable_after_sharing_body_data() {
        use std::io::Read;
        let mut chunks = Chunks::new(vec![
            Bytes::from_static(b"hello "),
            Bytes::from_static(b"world"),
        ]);
        let shared = chunks.body_data().expect("non-empty body");
        let mut cursor = BodyCursor::new(shared);
        let mut from_cursor = [0u8; 16];
        let (n, eof) = cursor.read(&mut from_cursor);
        assert!(eof);
        assert_eq!(&from_cursor[..n], b"hello world");

        let mut from_read = Vec::new();
        chunks.read_to_end(&mut from_read).unwrap();
        assert_eq!(from_read, b"hello world");
    }

    #[test]
    fn failed_body_cursor_seek_keeps_existing_position() {
        struct Case {
            name: &'static str,
            offset: i64,
            whence: i32,
        }
        let cases = [
            Case {
                name: "negative SEEK_SET fails without moving cursor",
                offset: -1,
                whence: libc::SEEK_SET,
            },
            Case {
                name: "negative SEEK_CUR past start fails without moving cursor",
                offset: -10,
                whence: libc::SEEK_CUR,
            },
            Case {
                name: "unknown whence fails without moving cursor",
                offset: 0,
                whence: 99,
            },
        ];
        for case in cases {
            let mut chunks = Chunks::new(vec![
                Bytes::from_static(b"hello "),
                Bytes::from_static(b"world"),
            ]);
            let mut cursor = BodyCursor::new(chunks.body_data().expect("non-empty body"));
            let mut head = [0u8; 6];
            assert_eq!(cursor.read(&mut head), (6, false), "{}", case.name);
            assert_eq!(cursor.seek(case.offset, case.whence), None, "{}", case.name);
            let mut tail = [0u8; 5];
            assert_eq!(cursor.read(&mut tail), (5, true), "{}", case.name);
            assert_eq!(&tail, b"world", "{}", case.name);
        }
    }
}
