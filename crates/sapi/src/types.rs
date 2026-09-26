use crate::api::{Addr, Tls};
use bytes::Bytes;
use http::header::{AUTHORIZATION, COOKIE, HeaderMap, HeaderName};
use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::path::PathBuf;
use tokio::sync::mpsc::{Sender, error::TrySendError};

#[derive(Debug, Clone)]
pub enum Mode {
    Classic,
    Worker(PathBuf),
    Dispatcher(PathBuf),
}

/// One service of a gRPC pool. `name` is fully qualified.
#[derive(Debug, Clone)]
pub struct GrpcService {
    pub name: String,
    /// In descriptor order.
    pub methods: Vec<GrpcMethod>,
}

/// One method of a `GrpcService`. `name` is the bare method name; the two types are fully qualified message names.
#[derive(Debug, Clone)]
pub struct GrpcMethod {
    pub name: String,
    pub input_type: String,
    pub output_type: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

/// `Rapira\Grpc\MethodKind`: client streaming streams the request, server streaming streams the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodKind {
    Unary,
    ServerStreaming,
    ClientStreaming,
    BidiStreaming,
}

impl MethodKind {
    pub(crate) fn of(m: &GrpcMethod) -> Self {
        match (m.client_streaming, m.server_streaming) {
            (false, false) => Self::Unary,
            (false, true) => Self::ServerStreaming,
            (true, false) => Self::ClientStreaming,
            (true, true) => Self::BidiStreaming,
        }
    }

    /// From the backing value of a case.
    pub(crate) fn from_value(value: &[u8]) -> Option<Self> {
        Some(match value {
            b"unary" => Self::Unary,
            b"server-streaming" => Self::ServerStreaming,
            b"client-streaming" => Self::ClientStreaming,
            b"bidi-streaming" => Self::BidiStreaming,
            _ => return None,
        })
    }

    pub(crate) fn case(self) -> &'static CStr {
        match self {
            Self::Unary => c"Unary",
            Self::ServerStreaming => c"ServerStreaming",
            Self::ClientStreaming => c"ClientStreaming",
            Self::BidiStreaming => c"BidiStreaming",
        }
    }

    pub(crate) fn streams_request(self) -> bool {
        matches!(self, Self::ClientStreaming | Self::BidiStreaming)
    }

    pub(crate) fn streams_response(self) -> bool {
        matches!(self, Self::ServerStreaming | Self::BidiStreaming)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok = 0,
    Bailout = 1,
    Throw = 3,
}

impl Outcome {
    pub fn from_c(v: c_int) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::Bailout,
            3 => Self::Throw,
            _ => Self::Bailout,
        }
    }
}

/// A well-formed response stream is `Interim* Head? (Chunk|File)* End?`; a channel closing without `End` means the producer died.
pub enum Frame {
    /// Advisory interim head (100-199, never 101).
    Interim(ResponseHead),
    Head {
        head: ResponseHead,
        /// Some: the framing the consumer must apply; None: the consumer chooses (chunked on HTTP/1.1).
        content_length: Option<u64>,
        /// 204, 304, 1xx or a HEAD request: no body bytes and no framing fields on the wire.
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

/// `unlink` is the one remover: seal calls it at finalize, Drop is the abnormal-path net.
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

pub enum Body {
    Raw(std::io::Cursor<Vec<u8>>),
    Multipart(MultipartBody),
}

pub struct Request {
    pub method: String,
    pub uri: String,
    /// Request-target bytes; a front can reconstruct them from its parsed URI. None falls back to `uri`'s bytes.
    pub target: Option<Vec<u8>>,
    /// Byte-for-byte as the client named it; None = the client named none.
    pub authority: Option<Vec<u8>>,
    pub https: bool,
    /// Wire/CGI spelling ("HTTP/2.0"); mapped to the contract spelling at the exchange view.
    pub protocol: String,
    pub remote: Addr,
    pub server: Addr,
    /// Configured CGI SERVER_NAME/SERVER_PORT, also the $uri synthesis fallback.
    pub server_name: String,
    pub server_port: u16,
    pub headers: HeaderMap,
    /// First content-type field line, raw bytes.
    pub content_type: Option<Vec<u8>>,
    /// Wire byte count, never re-derived from parsed parts; -1 if unknown.
    pub content_length: i64,
    pub body: Body,
    /// Unix seconds; None = not yet stamped.
    pub received_at: Option<f64>,
    pub tls: Option<Tls>,
}

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

fn cgi_addr_strings(addr: &Addr) -> (String, String) {
    match addr {
        Addr::Inet(sa) => (sa.ip().to_string(), sa.port().to_string()),
        // REMOTE_ADDR must hold a hostnumber (RFC 3875 §4.1.8); loopback stands in for a unix peer.
        // https://www.rfc-editor.org/rfc/rfc3875#section-4.1.8
        Addr::Unix(_) => ("127.0.0.1".to_owned(), "0".to_owned()),
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

        let (remote_addr, remote_port) = cgi_addr_strings(&r.remote);

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
    pub req: Request,
    /// None on the dispatcher path, which binds no server context and runs no CGI callbacks.
    pub c: Option<ReqC>,
    pub sender: Option<Sender<Frame>>,
    pub head: Option<ResponseHead>,
    pub body: Vec<u8>,
    pub stream: StreamState,
    pub tearing_down: bool,
}

impl Context {
    pub fn new(req: Request, sender: Sender<Frame>, superglobals: bool) -> Self {
        let c = superglobals.then(|| ReqC::build(&req));
        Self {
            req,
            c,
            sender: Some(sender),
            head: None,
            body: Vec::new(),
            stream: StreamState::NotSent,
            tearing_down: false,
        }
    }

    pub fn is_truncated(&self, errored: bool) -> bool {
        errored && self.stream == StreamState::BodyStreamed
    }

    pub fn commit_head(&mut self, mut status: u16, mut headers: HeaderMap) {
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
    pub fn finish(&mut self, truncated: bool) {
        let Some(tx) = self.sender.take() else {
            return;
        };
        let body = std::mem::take(&mut self.body);
        if let Some(head) = self.head.take() {
            let bodiless = matches!(head.status, 204 | 304)
                || (100..200).contains(&head.status)
                || self.req.method.eq_ignore_ascii_case("HEAD");
            let content_length = (!bodiless && !truncated).then_some(body.len() as u64);
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
mod tests {
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

    fn head_of(status: u16, headers: &[(&str, &str)]) -> ResponseHead {
        let mut ctx = Context {
            req: Request {
                method: String::new(),
                uri: String::new(),
                target: None,
                authority: None,
                https: false,
                protocol: String::new(),
                remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
                server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
                server_name: String::new(),
                server_port: 8080,
                headers: HeaderMap::new(),
                content_type: None,
                content_length: -1,
                body: Body::Raw(std::io::Cursor::new(Vec::new())),
                received_at: None,
                tls: None,
            },
            c: None,
            sender: None,
            head: None,
            body: Vec::new(),
            stream: StreamState::NotSent,
            tearing_down: false,
        };
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
}
