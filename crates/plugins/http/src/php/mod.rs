use std::{
    ffi::{CStr, CString, c_char, c_void},
    net::SocketAddr,
    time::{Duration, Instant},
};

use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use rapira_sapi::callbacks::{MAX_BUFFERED_BODY, guard};
use rapira_sapi::exchange::{
    AddrOwned, add_list, build_address, header_key, note_served, path_bytes, push_dec, push_ipv4,
};
use rapira_sapi::plugin::PhpPart;
use rapira_sapi::scoreboard::{Event, sb_update};
use rapira_sapi::types::{
    Addr, Body, Context, FormField, Frame, Request, ResponseHead, Tls, UploadedFile,
};
use rapira_sapi::work::{DispatcherClasses, Held, release};
use rapira_sapi::{
    HashPosition, HashTable, IS_ARRAY, IS_STRING, add_next_index_object, object_init_ex,
    rapira_array_init, rapira_ce_already_finalized_error, rapira_ce_tls,
    rapira_ce_work_discarded_exception, rapira_eg, zend, zend_class_entry,
    zend_hash_get_current_data_ex, zend_hash_get_current_key_ex,
    zend_hash_internal_pointer_reset_ex, zend_hash_move_forward_ex, zend_object, zend_set_timeout,
    zend_string, zend_unset_timeout, zval, zval_add_ref, zval_ptr_dtor,
};
use tokio::sync::mpsc::{Sender, error::TrySendError};

mod headers;
mod request;
mod respond;
mod sendfile;
#[cfg(test)]
mod tests;
mod values;

pub use sendfile::set_sendfile_root;

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
    fn rapira_http_register_classes();
}

/// Mirrors `rapira_exchange_obj` in rapira_http.h. The C fields sit before `std`.
#[repr(C)]
pub struct ExchangeObj {
    pub job: *mut c_void,
    pub request: zval,
    pub std: zend_object,
}

pub static DISPATCHER_CLASSES: DispatcherClasses = DispatcherClasses {
    dispatcher: || unsafe { rapira_ce_internal_http_dispatcher },
    info: || unsafe { rapira_ce_internal_http_dispatcher_info },
    unit: || unsafe { rapira_ce_internal_http_exchange },
    busy: c"receive() while a Rapira\\Http\\Exchange is unfinalized; finalize it first",
};

pub static PHP_PART: PhpPart = PhpPart {
    register: rapira_http_register_classes,
    dispatcher: Some(DISPATCHER_CLASSES),
};

/// The head locks on the first head or body write: a body chunk commits an implicit 200 first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    HeadCommitted,
    Finalized,
}

/// A committed head, not yet on the wire: the bytes leave with the first body-touching verb.
struct PendingHead {
    status: u16,
    headers: HeaderMap,
}

enum BodyState {
    Raw(Vec<u8>),
    /// Spool files unlink at seal(); Drop is the abnormal-path net.
    Multipart {
        fields: Vec<FieldPart>,
        files: Vec<FilePart>,
    },
}

struct FieldPart {
    field: FormField,
    headers: Grouped,
}

struct FilePart {
    upload: UploadedFile,
    path: Vec<u8>,
    headers: Grouped,
}

/// Multipart part headers, one entry per name. Keys are CStrings: the symtable prefilter in add_assoc_zval_ex reads one byte past a leading `-`, which the terminator covers.
struct Grouped(Vec<(CString, Vec<Vec<u8>>)>);

impl Grouped {
    fn new(headers: &[(String, Vec<u8>)]) -> Self {
        let mut out: Vec<(CString, Vec<Vec<u8>>)> = Vec::new();
        for (name, value) in headers {
            let nb = name.as_bytes();
            if nb.is_empty() {
                continue;
            }
            match out.iter_mut().find(|(n, _)| n.as_bytes() == nb) {
                Some((_, values)) => values.push(value.clone()),
                None => {
                    let Ok(key) = CString::new(nb) else { continue };
                    out.push((key, vec![value.clone()]));
                }
            }
        }
        Self(out)
    }
}

/// Contract spelling for `Request::$protocol`: HTTP/2, not the CGI HTTP/2.0.
fn protocol_php(protocol: &str) -> &str {
    match protocol {
        "HTTP/2.0" => "HTTP/2",
        "HTTP/3.0" => "HTTP/3",
        p => p,
    }
}

/// Owned values of the `Request` object. The builder keeps them in the state because a Zend OOM bailout longjmps through its frame, so that frame holds no values with Drop glue.
struct RequestView {
    uri_abs: String,
    remote: AddrOwned,
    server: AddrOwned,
}

impl RequestView {
    fn new(req: &Request) -> Self {
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
                    push_dec(&mut uri_abs, sa.port());
                }
                Addr::Inet(sa) => uri_abs.push_str(&sa.to_string()),
                Addr::Unix(_) => {
                    uri_abs.push_str(&req.server_name);
                    uri_abs.push(':');
                    push_dec(&mut uri_abs, req.server_port);
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
    // body above ctx: declaration drop order unlinks the spool files before the frame sender closes
    body: BodyState,
    ctx: Context,
    /// Filled by the first `getRequest()`.
    view: Option<RequestView>,
    stage: Stage,
    head_sent: bool,
    pending: Option<PendingHead>,
    declared_cl: Option<u64>,
    /// Bytes accepted toward `declared_cl`: bodiless units count too, so a HEAD handler hits the same errors.
    sent_body: u64,
    discarded: bool,
    /// 204, 304, 101, or a HEAD request: chunks are accepted and dropped.
    bodiless: bool,
    /// Last wall-timer arm: the park guard re-arms the remaining budget.
    armed_at: Instant,
}

impl ExchangeState {
    pub(crate) fn new(req: Request, tx: Sender<Frame>) -> Self {
        let mut ctx = Context::new(req, tx, false);
        let taken = std::mem::replace(
            &mut ctx.req.body,
            Body::Raw(std::io::Cursor::new(Vec::new())),
        );
        let body = match taken {
            Body::Raw(cursor) => BodyState::Raw(cursor.into_inner()),
            Body::Multipart(mb) => BodyState::Multipart {
                fields: mb
                    .fields
                    .into_iter()
                    .map(|f| FieldPart {
                        headers: Grouped::new(&f.headers),
                        field: f,
                    })
                    .collect(),
                files: mb
                    .files
                    .into_iter()
                    .map(|f| FilePart {
                        path: path_bytes(&f.file.path),
                        headers: Grouped::new(&f.headers),
                        upload: f,
                    })
                    .collect(),
            },
        };
        let bodiless = ctx.req.method.eq_ignore_ascii_case("HEAD");

        Self {
            ctx,
            body,
            view: None,
            stage: Stage::Open,
            head_sent: false,
            pending: None,
            declared_cl: None,
            sent_body: 0,
            discarded: false,
            bodiless,
            armed_at: Instant::now(),
        }
    }
}

impl Held for ExchangeState {
    fn finalized(&self) -> bool {
        self.stage == Stage::Finalized
    }

    fn client_closed(&self) -> bool {
        self.discarded
            || (self.stage != Stage::Finalized
                && self.ctx.sender.as_ref().is_some_and(Sender::is_closed))
    }

    fn discard(&mut self) {
        respond::discard_unit(self);
    }
}

rapira_sapi::container_of!(pub(crate) exchange_from, ExchangeObj);
