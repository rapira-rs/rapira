//! This module has the Rust code behind the PHP classes of the http plugin: the class entries and the exchange state.

use std::{
    ffi::{CStr, c_char},
    fmt::NumBuffer,
    net::{IpAddr, SocketAddr},
};

use http::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::body::Bytes;
use rapira_sapi::callbacks::guard;
use rapira_sapi::exchange::{add_list, build_address, header_key, ip_text};
use rapira_sapi::pool::{Event, record};
use rapira_sapi::types::{Addr, Frame, Request, ResponseHead};
use rapira_sapi::work::{Held, HeldPtr};
use rapira_sapi::{
    HashTable, IS_ARRAY, IS_STRING, add_next_index_object, object_init_ex, rapira_array_init,
    rapira_ce_already_finalized_error, rapira_ce_work_discarded_exception, rapira_plugin, zend,
    zend_class_entry, zval, zval_ptr_dtor,
};
use tokio::sync::mpsc::Sender;

use crate::multipart::{HeaderValues, MultipartBody, UploadedFile};

mod headers;
mod request;
mod respond;
mod sendfile;
#[cfg(test)]
mod tests;

pub(crate) use sendfile::set_sendfile_root;

// The class entries of the http stub. rapira_http_register_classes assigns them in MINIT (rapira_http.h).
unsafe extern "C" {
    pub static mut rapira_ce_http_multipart: *mut zend_class_entry;
    pub static mut rapira_ce_http_form_field: *mut zend_class_entry;
    pub static mut rapira_ce_http_uploaded_file: *mut zend_class_entry;
    pub static mut rapira_ce_http_request: *mut zend_class_entry;
    pub static mut rapira_ce_http_head_already_written_error: *mut zend_class_entry;
    pub static mut rapira_ce_http_head_not_written_error: *mut zend_class_entry;
    pub static mut rapira_ce_http_content_length_exceeded_error: *mut zend_class_entry;
    pub static mut rapira_ce_http_file_not_sendable_exception: *mut zend_class_entry;
    // The unit class. It is the class of the object that receive() creates for an ExchangeState.
    static mut rapira_ce_http_exchange: *mut zend_class_entry;
    // The record of the http plugin (rapira_http_classes.c).
    pub static mut rapira_http_plugin: rapira_plugin;
    pub fn rapira_http_register_classes();
}

/// The head locks on the first head or body write. A body write sends an implicit 200 head first.
enum Phase {
    /// No head is written.
    Open,
    /// `writeHead()` stored the final head, and it is not sent.
    Committed(ResponseHead),
    /// The head frame went out.
    Sent,
    /// The End went out.
    Sealed,
    /// The unit closed before its End went out.
    Discarded,
}

/// Returns the contract spelling of `Request::$protocol`, for example HTTP/2, not the CGI HTTP/2.0.
fn protocol_php(protocol: &str) -> &str {
    match protocol {
        "HTTP/2.0" => "HTTP/2",
        p => p,
    }
}

/// The `$uri` of the `Request` object.
pub fn uri_abs(req: &Request) -> String {
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
                uri_abs.push_str(ip_text(IpAddr::V4(*sa.ip()), &mut [0; _]));
                uri_abs.push(':');
                uri_abs.push_str(sa.port().format_into(&mut NumBuffer::new()));
            }
            Addr::Inet(sa) => uri_abs.push_str(&sa.to_string()),
            Addr::Unix(_) => {
                let identity = rapira_sapi::HttpIdentity::current();
                uri_abs.push_str(identity.name());
                uri_abs.push(':');
                uri_abs.push_str(identity.port.format_into(&mut NumBuffer::new()));
            }
        },
    }
    uri_abs.push_str(path);
    uri_abs
}

pub struct ExchangeState {
    // The multipart field comes before sender, so the drop in declaration order unlinks the spool files before the frame sender closes.
    /// The multipart parser takes the raw chunks from `req`. For other bodies, `req` keeps the chunks until `getRequest()` stores the PHP object. seal(), discard_unit() and fail() unlink the spool files. SpooledFile::drop removes a file that a bailout after a fatal error or a timeout leaves behind.
    multipart: Option<MultipartBody>,
    req: Request,
    sender: Option<Sender<Frame>>,
    /// The first `getRequest()` fills it. The state keeps the owned `$uri` because a Zend OOM bailout longjmps through the builder frame, so that frame holds no values with Drop glue.
    uri_abs: Option<String>,
    phase: Phase,
    declared_cl: Option<u64>,
    /// The bytes accepted toward `declared_cl`. Bodiless units count too, so a HEAD handler gets the same errors.
    sent_body: u64,
    /// True for 204, 304, 101 or a HEAD request. The state then accepts chunks and drops them.
    bodiless: bool,
}

impl ExchangeState {
    pub(crate) fn new(req: Request, multipart: Option<MultipartBody>, tx: Sender<Frame>) -> Self {
        let bodiless = req.method == "HEAD";

        Self {
            multipart,
            req,
            sender: Some(tx),
            uri_abs: None,
            phase: Phase::Open,
            declared_cl: None,
            sent_body: 0,
            bodiless,
        }
    }

    fn unlink_spools(&mut self) {
        if let Some(body) = &mut self.multipart {
            for f in &mut body.files {
                f.file.unlink();
            }
        }
    }

    /// The seal of a unit that PHP does not answer. It sends a `status` head and an End when no head was sent, else a truncated End.
    fn fail(&mut self, status: u16) {
        self.unlink_spools();
        let Some(tx) = self.sender.take() else {
            return;
        };
        if matches!(self.phase, Phase::Sent) {
            let _ = tx.try_send(Frame::End { truncated: true });
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
            let _ = tx.try_send(Frame::End { truncated: false });
        }
    }
}

impl Held for ExchangeState {
    fn settled(&self) -> bool {
        matches!(self.phase, Phase::Sealed | Phase::Discarded)
    }

    /// A finalized unit holds no sender, so only an open one can see the client go.
    fn cancelled(&self) -> bool {
        matches!(self.phase, Phase::Discarded)
            || self.sender.as_ref().is_some_and(Sender::is_closed)
    }

    fn discard(&mut self) {
        respond::discard_unit(self);
    }

    /// Seals the unit with a 500.
    fn lost(&mut self) {
        self.fail(500);
    }

    /// Seals the unit with a 503 after a failed boot.
    fn shed(mut self: Box<Self>) {
        self.fail(503);
    }

    fn class(&self) -> *mut zend_class_entry {
        // SAFETY: MINIT sets the class entry before any worker starts.
        unsafe { rapira_ce_http_exchange }
    }
}
