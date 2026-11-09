//! The Rust behind the PHP classes of the http plugin: the class entries, the exchange state and the request view.

use std::{
    ffi::{CStr, c_char},
    fmt::NumBuffer,
    net::SocketAddr,
};

use http::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::body::Bytes;
use rapira_sapi::callbacks::guard;
use rapira_sapi::exchange::{AddrOwned, add_list, build_address, header_key, push_ipv4};
use rapira_sapi::scoreboard::{Event, sb_update};
use rapira_sapi::types::{Addr, Chunks, Frame, Request, ResponseHead};
use rapira_sapi::work::{Held, HeldPtr};
use rapira_sapi::{
    HashTable, IS_ARRAY, IS_STRING, add_next_index_object, object_init_ex, rapira_array_init,
    rapira_ce_already_finalized_error, rapira_ce_work_discarded_exception, rapira_plugin, zend,
    zend_class_entry, zval, zval_ptr_dtor,
};
use tokio::sync::mpsc::{Sender, error::TrySendError};

use crate::multipart::{HeaderValues, MultipartBody, UploadedFile};

mod headers;
mod request;
mod respond;
mod sendfile;
#[cfg(test)]
mod tests;

pub(crate) use sendfile::set_sendfile_root;

// Class entries of the http stub; rapira_http_register_classes assigns them in MINIT (rapira_http.h).
unsafe extern "C" {
    pub static mut rapira_ce_http_multipart: *mut zend_class_entry;
    pub static mut rapira_ce_http_form_field: *mut zend_class_entry;
    pub static mut rapira_ce_http_uploaded_file: *mut zend_class_entry;
    pub static mut rapira_ce_http_request: *mut zend_class_entry;
    pub static mut rapira_ce_http_head_already_written_error: *mut zend_class_entry;
    pub static mut rapira_ce_http_head_not_written_error: *mut zend_class_entry;
    pub static mut rapira_ce_http_content_length_exceeded_error: *mut zend_class_entry;
    pub static mut rapira_ce_http_file_not_sendable_exception: *mut zend_class_entry;
    // The dispatcher surface of the http plugin (rapira_http_classes.c).
    pub static mut rapira_http_plugin: rapira_plugin;
    pub fn rapira_http_register_classes();
    // The server identity that DispatcherInfo::serverName() and serverPort() return (rapira_http.h).
    static mut rapira_http_server_name: *const c_char;
    static mut rapira_http_server_name_len: usize;
    static mut rapira_http_server_port: rapira_sapi::zend_long;
}

/// Runs once in the master before the fork; the workers inherit the values.
pub(crate) fn set_server_identity(name: &'static str, port: u16) {
    // SAFETY: the master is single-threaded and no PHP reads the values before the fork; name is static, so the pointer stays valid.
    unsafe {
        rapira_http_server_name = zend::ptr_or_empty(name.as_bytes());
        rapira_http_server_name_len = name.len();
        rapira_http_server_port = i64::from(port);
    }
}

/// The head locks on the first head or body write: a body chunk commits an implicit 200 first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    HeadCommitted,
    Finalized,
}

enum BodyState {
    Raw(Chunks),
    /// seal(), discard_unit() and fail() unlink the spool files; SpooledFile::drop removes a file that a bailout (fatal or timeout) leaves behind.
    Multipart(MultipartBody),
}

impl BodyState {
    fn unlink_spools(&mut self) {
        if let Self::Multipart(body) = self {
            for f in &mut body.files {
                f.file.unlink();
            }
        }
    }
}

/// Contract spelling for `Request::$protocol`: HTTP/2, not the CGI HTTP/2.0.
fn protocol_php(protocol: &str) -> &str {
    match protocol {
        "HTTP/2.0" => "HTTP/2",
        p => p,
    }
}

/// Owned values of the `Request` object. The builder keeps them in the state because a Zend OOM bailout longjmps through its frame, so that frame holds no values with Drop glue.
pub struct RequestView {
    pub uri_abs: String,
    remote: AddrOwned,
    server: AddrOwned,
}

impl RequestView {
    pub fn new(req: &Request) -> Self {
        let scheme = if req.https { "https://" } else { "http://" };
        let path = if req.uri.starts_with('/') {
            req.uri.as_str()
        } else {
            "/"
        };
        // 21 bytes hold the longest IPv4 host, "255.255.255.255:65535".
        let host_len = req.authority.as_ref().map_or(21, Vec::len);
        let mut uri_abs = String::with_capacity(scheme.len() + host_len + path.len());
        uri_abs.push_str(scheme);
        match &req.authority {
            Some(a) => uri_abs.push_str(&String::from_utf8_lossy(a)),
            None => match &req.server {
                Addr::Inet(SocketAddr::V4(sa)) => {
                    push_ipv4(&mut uri_abs, *sa.ip());
                    uri_abs.push(':');
                    uri_abs.push_str(sa.port().format_into(&mut NumBuffer::new()));
                }
                Addr::Inet(sa) => uri_abs.push_str(&sa.to_string()),
                Addr::Unix(_) => {
                    uri_abs.push_str(req.server_name);
                    uri_abs.push(':');
                    uri_abs.push_str(req.server_port.format_into(&mut NumBuffer::new()));
                }
            },
        }
        uri_abs.push_str(path);
        Self {
            uri_abs,
            remote: AddrOwned::new(&req.remote),
            server: AddrOwned::new(&req.server),
        }
    }
}

pub struct ExchangeState {
    // body above sender: declaration drop order unlinks the spool files before the frame sender closes
    body: BodyState,
    req: Request,
    sender: Option<Sender<Frame>>,
    /// Filled by the first `getRequest()`.
    view: Option<RequestView>,
    stage: Stage,
    head_sent: bool,
    pending: Option<ResponseHead>,
    declared_cl: Option<u64>,
    /// Bytes accepted toward `declared_cl`: bodiless units count too, so a HEAD handler hits the same errors.
    sent_body: u64,
    discarded: bool,
    /// 204, 304, 101, or a HEAD request: chunks are accepted and dropped.
    bodiless: bool,
}

impl ExchangeState {
    pub(crate) fn new(
        mut req: Request,
        multipart: Option<MultipartBody>,
        tx: Sender<Frame>,
    ) -> Self {
        let body = match multipart {
            Some(parsed) => BodyState::Multipart(parsed),
            None => BodyState::Raw(std::mem::take(&mut req.body)),
        };
        let bodiless = req.method == "HEAD";

        Self {
            body,
            req,
            sender: Some(tx),
            view: None,
            stage: Stage::Open,
            head_sent: false,
            pending: None,
            declared_cl: None,
            sent_body: 0,
            discarded: false,
            bodiless,
        }
    }

    /// The seal of a unit PHP does not answer: a `status` head and an End when no head was sent, else a truncated End.
    fn fail(&mut self, status: u16) {
        self.body.unlink_spools();
        let Some(tx) = self.sender.take() else {
            return;
        };
        if self.head_sent {
            let _ = tx.try_send(Frame::End {
                trailers: HeaderMap::new(),
                truncated: true,
            });
        } else if tx
            .try_send(Frame::Head {
                head: ResponseHead {
                    status,
                    headers: HeaderMap::new(),
                },
                content_length: (!self.bodiless).then_some(0),
                bodiless: self.bodiless,
            })
            .is_ok()
        {
            let _ = tx.try_send(Frame::End {
                trailers: HeaderMap::new(),
                truncated: false,
            });
        }
    }
}

impl Held for ExchangeState {
    fn settled(&self) -> bool {
        self.stage == Stage::Finalized
    }

    /// A finalized unit holds no sender, so only an open one can see the client go.
    fn cancelled(&self) -> bool {
        self.discarded || self.sender.as_ref().is_some_and(Sender::is_closed)
    }

    fn discard(&mut self) {
        respond::discard_unit(self);
    }

    /// The 500 seal.
    fn lost(&mut self) {
        self.fail(500);
    }

    /// The 503 seal of a failed boot.
    fn shed(mut self: Box<Self>) {
        self.fail(503);
    }
}
