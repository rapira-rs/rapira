//! This module has the Rust code behind the PHP classes of the http plugin: the class entries and the exchange state.

use std::{
    ffi::{CStr, c_char},
    fmt::NumBuffer,
    net::{IpAddr, SocketAddr},
};

use http::header::{HeaderName, HeaderValue};
use rapira_http_middleware::{ResponseHead, ResponseHeaders};
use rapira_sapi::callbacks::guard;
use rapira_sapi::exchange::{add_list_with_keys, build_address, header_keys, ip_text};
use rapira_sapi::pool::{Event, record};
use rapira_sapi::types::{Addr, Request};
use rapira_sapi::work::{Held, HeldPtr};
use rapira_sapi::{
    HashTable, IS_ARRAY, IS_STRING, add_next_index_object, object_init_ex, rapira_array_init,
    rapira_ce_already_finalized_error, rapira_ce_work_discarded_exception, rapira_plugin, zend,
    zend_class_entry, zval,
};

use crate::multipart::{HeaderRef, MultipartBody, UploadedFile};
use crate::responder::HttpResponder;

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
    /// The head went out onto the wire.
    Sent,
    /// The exchange finished cleanly.
    Sealed,
    /// The unit closed before finishing.
    Discarded,
}

/// Returns the contract spelling of `Request::$protocol`, for example HTTP/2, not the CGI HTTP/2.0.
fn protocol_php(protocol: &str) -> &str {
    match protocol {
        "HTTP/2.0" => "HTTP/2",
        p => p,
    }
}

/// Formats `$uri` into `push`, returning false if `push` rejects a slice (for example stack buffer full).
pub(crate) fn write_uri_abs(
    req: &Request,
    authority: Option<&[u8]>,
    mut push: impl FnMut(&[u8]) -> bool,
) -> bool {
    let scheme = if req.https {
        b"https://".as_slice()
    } else {
        b"http://".as_slice()
    };
    if !push(scheme) {
        return false;
    }
    match authority {
        Some(a) => match std::str::from_utf8(a) {
            Ok(s) => {
                if !push(s.as_bytes()) {
                    return false;
                }
            }
            Err(_) => {
                if !push(String::from_utf8_lossy(a).as_bytes()) {
                    return false;
                }
            }
        },
        None => match &req.server {
            Addr::Inet(SocketAddr::V4(sa)) => {
                let mut ip_buf = [0u8; 39];
                let ip = ip_text(IpAddr::V4(*sa.ip()), &mut ip_buf);
                let mut port_buf = NumBuffer::new();
                if !push(ip.as_bytes())
                    || !push(b":")
                    || !push(sa.port().format_into(&mut port_buf).as_bytes())
                {
                    return false;
                }
            }
            Addr::Inet(sa) => {
                if !push(sa.to_string().as_bytes()) {
                    return false;
                }
            }
            Addr::Unix(_) => {
                let identity = rapira_sapi::HttpIdentity::current();
                let mut port_buf = NumBuffer::new();
                if !push(identity.name().as_bytes())
                    || !push(b":")
                    || !push(identity.port.format_into(&mut port_buf).as_bytes())
                {
                    return false;
                }
            }
        },
    }
    let uri_str = req.uri.as_str();
    let path = if uri_str.starts_with('/') {
        uri_str.as_bytes()
    } else {
        b"/"
    };
    push(path)
}

/// The `$uri` of the `Request` object.
pub fn uri_abs(req: &Request) -> String {
    let authority = req.authority_bytes();
    let host_len = authority.map_or(21, <[u8]>::len);
    let mut out = Vec::with_capacity(8 + host_len + req.uri.as_str().len().max(1));
    write_uri_abs(req, authority, |s| {
        out.extend_from_slice(s);
        true
    });
    // SAFETY: `write_uri_abs` pushes only ASCII literals, valid UTF-8, or lossy UTF-8.
    unsafe { String::from_utf8_unchecked(out) }
}

pub struct ExchangeState {
    // The multipart field comes before responder, so the drop in declaration order unlinks the spool files before the responder signals completion.
    multipart: Option<Box<MultipartBody>>,
    req: Request,
    responder: Option<HttpResponder>,
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
    pub(crate) fn new(
        req: Request,
        multipart: Option<Box<MultipartBody>>,
        responder: HttpResponder,
    ) -> Self {
        let bodiless = req.method == "HEAD";

        Self {
            multipart,
            req,
            responder: Some(responder),
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

    /// The seal of a unit that PHP does not answer. Sends `status` when no head was sent, else truncates the wire stream.
    fn fail(&mut self, status: u16) {
        self.unlink_spools();
        let Some(mut responder) = self.responder.take() else {
            return;
        };
        if matches!(self.phase, Phase::Sent) {
            unsafe { responder.finish(true) };
        } else {
            responder.send_error(status);
        }
    }
}

impl Held for ExchangeState {
    fn settled(&self) -> bool {
        matches!(self.phase, Phase::Sealed | Phase::Discarded)
    }

    /// A finalized unit holds no responder, so only an open one can see the client go.
    fn cancelled(&self) -> bool {
        matches!(self.phase, Phase::Discarded)
            || self
                .responder
                .as_ref()
                .is_some_and(HttpResponder::is_cancelled)
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
