//! The Rust behind the PHP classes of the http plugin: the class entries, the exchange state and the request view.

use std::{
    ffi::{CStr, c_char},
    fmt::NumBuffer,
    net::SocketAddr,
};

use http::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::body::Bytes;
use rapira_sapi::callbacks::guard;
use rapira_sapi::exchange::{
    AddrOwned, add_list, build_address, header_key, path_bytes, push_ipv4,
};
use rapira_sapi::scoreboard::{Event, sb_update};
use rapira_sapi::types::{Addr, Body, FormField, Frame, Request, ResponseHead, UploadedFile};
use rapira_sapi::work::{DispatcherClasses, Held, HeldPtr};
use rapira_sapi::{
    HashTable, IS_ARRAY, IS_STRING, add_next_index_object, object_init_ex, rapira_array_init,
    rapira_ce_already_finalized_error, rapira_ce_work_discarded_exception, zend, zend_class_entry,
    zend_object, zval, zval_add_ref, zval_ptr_dtor,
};
use tokio::sync::mpsc::{Sender, error::TrySendError};

mod headers;
mod info;
mod request;
mod respond;
mod sendfile;
#[cfg(test)]
mod tests;
mod values;

pub(crate) use info::set_server_identity;
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
    pub static mut rapira_ce_internal_http_dispatcher: *mut zend_class_entry;
    pub static mut rapira_ce_internal_http_dispatcher_info: *mut zend_class_entry;
    pub static mut rapira_ce_internal_http_exchange: *mut zend_class_entry;
    pub fn rapira_http_register_classes();
}

/// Mirrors `rapira_exchange_obj` in rapira_http.h. The C fields sit before `std`.
#[repr(C)]
pub struct ExchangeObj {
    pub held: HeldPtr,
    pub request: zval,
    pub std: zend_object,
}

pub static DISPATCHER_CLASSES: DispatcherClasses = DispatcherClasses {
    dispatcher: || unsafe { rapira_ce_internal_http_dispatcher },
    info: || unsafe { rapira_ce_internal_http_dispatcher_info },
    unit: || unsafe { rapira_ce_internal_http_exchange },
    busy: c"this worker holds one unit at a time, and its Rapira\\Http\\Exchange is unfinalized; finalize it first",
};

/// The head locks on the first head or body write: a body chunk commits an implicit 200 first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    HeadCommitted,
    Finalized,
}

enum BodyState {
    Raw(Vec<u8>),
    /// seal(), discard_unit() and lost() unlink the spool files; SpooledFile::drop removes a file that a bailout (fatal or timeout) leaves behind.
    Multipart {
        fields: Vec<FieldPart>,
        files: Vec<FilePart>,
    },
}

impl BodyState {
    fn unlink_spools(&mut self) {
        if let Self::Multipart { files, .. } = self {
            for p in files {
                p.upload.file.unlink();
            }
        }
    }
}

struct FieldPart {
    field: FormField,
    headers: Vec<HeaderValues>,
}

struct FilePart {
    upload: UploadedFile,
    path: Vec<u8>,
    headers: Vec<HeaderValues>,
}

/// Multipart part headers, one entry per name.
struct HeaderValues {
    name: String,
    values: Vec<Vec<u8>>,
}

fn group_headers(headers: Vec<(String, Vec<u8>)>) -> Vec<HeaderValues> {
    let mut out: Vec<HeaderValues> = Vec::new();
    for (name, value) in headers {
        match out.iter_mut().find(|header| header.name == name) {
            Some(header) => header.values.push(value),
            None => out.push(HeaderValues {
                name,
                values: vec![value],
            }),
        }
    }
    out
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
    pub(crate) fn new(mut req: Request, tx: Sender<Frame>) -> Self {
        let taken = std::mem::replace(&mut req.body, Body::Raw(std::io::Cursor::new(Vec::new())));
        let body = match taken {
            Body::Raw(cursor) => BodyState::Raw(cursor.into_inner()),
            Body::Multipart(mb) => {
                let mut fields = Vec::with_capacity(mb.fields.len());
                for mut field in mb.fields {
                    fields.push(FieldPart {
                        headers: group_headers(std::mem::take(&mut field.headers)),
                        field,
                    });
                }
                let mut files = Vec::with_capacity(mb.files.len());
                for mut upload in mb.files {
                    files.push(FilePart {
                        path: path_bytes(&upload.file.path),
                        headers: group_headers(std::mem::take(&mut upload.headers)),
                        upload,
                    });
                }
                BodyState::Multipart { fields, files }
            }
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

    /// A 500 head and an End when no head was sent, else a truncated End.
    fn lost(&mut self) {
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
                    status: 500,
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
