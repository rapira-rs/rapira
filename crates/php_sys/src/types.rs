use bytes::Bytes;
use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::os::raw::c_int;
use std::path::PathBuf;
use tokio::sync::mpsc::Sender;

#[derive(Debug, Clone)]
pub enum Mode {
    Classic,
    WorkerRequest(PathBuf),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok = 0,
    Bailout = 1,
    Exit = 2,
    Throw = 3,
}

impl Outcome {
    pub fn from_c(v: c_int) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::Bailout,
            2 => Self::Exit,
            3 => Self::Throw,
            _ => Self::Bailout,
        }
    }
}

pub struct Frame {
    pub head: Option<ResponseHead>,
    pub body: Bytes,
    pub truncated: bool,
}

pub struct Job {
    pub ctx: Context,
}

pub struct Request {
    pub method: String,
    pub uri: String,
    pub https: bool,
    pub query: String,
    pub protocol: String,
    pub remote_addr: String,
    pub server_name: String,
    pub server_port: String,
    pub remote_port: String,
    pub script_name: String,
    pub document_root: String,
    pub script_filename: PathBuf,
    pub headers: Vec<(String, Vec<u8>)>, // values as bytes: latin1/binary-safe
    pub server_vars: Vec<(String, String)>,
    pub content_type: Option<Vec<u8>>,
    pub content_length: i64, // -1 if unknown
    pub body: Box<dyn Read + Send>,
}

pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
}

fn status_field_code(value: &[u8]) -> Option<u16> {
    let digits: &[u8] = value
        .split(|b| b.is_ascii_whitespace())
        .find(|token| !token.is_empty())?;
    let code: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (100..=599).contains(&code).then_some(code)
}

pub struct ReqC {
    pub method: CString,
    pub query: CString,
    pub uri: CString,
    pub script: CString,
    pub ctype: Option<CString>,
    pub cookie: Option<CString>,
    pub authorization: Option<CString>,
    pub env: HashMap<Box<[u8]>, CString>,
}

impl ReqC {
    pub fn build(r: &Request) -> Self {
        let cookie: Option<Vec<u8>> = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .map(|(_, v)| v.clone());

        // Build the CStrings straight from the header bytes — no owned-String detour.
        let authorization: Option<CString> = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| CString::new(v.as_slice()).unwrap_or_default());

        let env: HashMap<Box<[u8]>, CString> = r
            .server_vars
            .iter()
            .filter_map(|(k, v)| Some((k.as_bytes().into(), CString::new(v.as_bytes()).ok()?)))
            .collect();

        Self {
            method: CString::new(r.method.as_bytes()).unwrap_or_default(),
            query: CString::new((r.query).as_bytes()).unwrap_or_default(),
            uri: CString::new(r.uri.as_bytes()).unwrap_or_default(),
            script: CString::new(r.script_filename.to_string_lossy().to_string())
                .unwrap_or_default(),
            cookie: cookie.map(|c| CString::new(c).unwrap_or_default()),
            authorization,
            ctype: r
                .content_type
                .as_deref()
                .map(|s| CString::new(s).unwrap_or_default()),
            env,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Nothing recorded yet.
    NotSent,
    /// A head has been recorded; no body yet.
    HeadSent,
    /// Body output began *during the handler* (not the teardown flush).
    BodyStreamed,
}

pub struct Context {
    pub req: Request,
    pub c: ReqC,
    pub sender: Option<Sender<Frame>>,
    pub head: Option<ResponseHead>,
    pub body: Vec<u8>,
    pub stream: StreamState,
    pub tearing_down: bool,
}

impl Context {
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
        }
    }

    pub fn is_truncated(&self, errored: bool) -> bool {
        errored && self.stream == StreamState::BodyStreamed
    }

    pub fn commit_head(&mut self, mut status: u16, mut headers: Vec<(String, Vec<u8>)>) {
        headers.retain(|(name, value)| {
            if !name.eq_ignore_ascii_case("status") {
                return true;
            }
            match status_field_code(value) {
                Some(code) => status = code,
                // Dropped either way, so without this the app sees its 404 silently served
                // as a 200 with nothing logged anywhere.
                None => tracing::warn!(
                    target: "php",
                    "ignored malformed Status field {:?}; status stays {status}",
                    String::from_utf8_lossy(value)
                ),
            }
            false
        });
        self.head = Some(ResponseHead { status, headers });
        self.stream = StreamState::HeadSent;
    }

    pub fn finish(&mut self, truncated: bool) {
        if let Some(tx) = self.sender.take() {
            let _ = tx.blocking_send(Frame {
                head: self.head.take(),
                body: std::mem::take(&mut self.body).into(),
                truncated,
            });
        }
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

    /// An unparseable or out-of-range value leaves the status alone. The caller drops the
    /// field either way, so a bogus one is discarded rather than shown to the client.
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
                https: false,
                query: String::new(),
                protocol: String::new(),
                remote_addr: String::new(),
                server_name: String::new(),
                server_port: String::new(),
                remote_port: String::new(),
                script_name: String::new(),
                document_root: String::new(),
                script_filename: PathBuf::new(),
                headers: Vec::new(),
                server_vars: Vec::new(),
                content_type: None,
                content_length: -1,
                body: Box::new(std::io::empty()),
            },
            c: ReqC {
                method: CString::default(),
                query: CString::default(),
                uri: CString::default(),
                script: CString::default(),
                ctype: None,
                cookie: None,
                authorization: None,
                env: HashMap::new(),
            },
            sender: None,
            head: None,
            body: Vec::new(),
            stream: StreamState::NotSent,
            tearing_down: false,
        };
        let headers = headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
            .collect();
        ctx.commit_head(status, headers);
        ctx.head.expect("commit_head records a head")
    }

    /// The whole `Status:` feature lives in commit_head's retain closure: the field sets the
    /// code, never reaches the client, and the match is case-insensitive because php-src
    /// passes whatever spelling the script wrote.
    #[test]
    fn commit_head_consumes_the_status_field() {
        let head = head_of(200, &[("status", "404 Not Found"), ("X-Keep", "kept")]);
        assert_eq!(head.status, 404);
        assert_eq!(head.headers.len(), 1);
        assert_eq!(head.headers[0].0, "X-Keep");
    }

    /// A `Status:` the SAPI cannot parse must still be consumed — forwarding it would put a
    /// literal `Status:` field on the wire — but it must not invent a code.
    #[test]
    fn commit_head_drops_an_unparseable_status_without_changing_the_code() {
        let head = head_of(201, &[("Status", "NotFound")]);
        assert_eq!(head.status, 201);
        assert!(head.headers.is_empty());
    }

    /// RFC 3875 §6.3.3 makes `Status` the script's own result code, and §6.2.1 puts the
    /// conversion on the server — so it is authoritative over whatever code php-src had
    /// already recorded from `http_response_code()`.
    #[test]
    fn commit_head_lets_the_status_field_override_the_recorded_code() {
        assert_eq!(head_of(500, &[("Status", "404")]).status, 404);
    }
}
