pub(crate) use std::{
    cell::{Cell, RefCell},
    ffi::{CStr, CString, c_char, c_int, c_void},
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

pub(crate) use bytes::Bytes;
pub(crate) use tokio::sync::mpsc::{Sender, error::TrySendError};

pub(crate) use crate::{
    HashPosition, HashTable, IS_ARRAY, IS_STRING, RAPIRA_MODE_DISPATCHER, add_assoc_zval_ex,
    add_next_index_object,
    callbacks::{MAX_BUFFERED_BODY, guard, is_field_value_byte, is_tchar},
    object_init_ex, rapira_array_init, rapira_ce_already_finalized_error,
    rapira_ce_closed_exception, rapira_ce_http_content_length_exceeded_error,
    rapira_ce_http_file_not_sendable_exception, rapira_ce_http_form_field,
    rapira_ce_http_head_already_written_error, rapira_ce_http_head_not_written_error,
    rapira_ce_http_multipart, rapira_ce_http_request, rapira_ce_http_tls,
    rapira_ce_http_uploaded_file, rapira_ce_inet_address, rapira_ce_internal_http_dispatcher,
    rapira_ce_internal_http_dispatcher_info, rapira_ce_internal_http_exchange,
    rapira_ce_no_dispatcher_error, rapira_ce_timeout_exception, rapira_ce_unix_address,
    rapira_ce_work_discarded_exception, rapira_dispatcher_info_obj, rapira_eg, rapira_exchange_obj,
    rapira_receive_timed, rapira_receive_untimed,
    scoreboard::{Event, sb_update},
    start::{Pulled, pending_depth, pull_job_try, pull_job_wait},
    types::{Addr, Body, FieldLines, FormField, Frame, Job, ResponseHead, TlsView, UploadedFile},
    zend, zend_class_entry, zend_hash_get_current_data_ex, zend_hash_get_current_key_ex,
    zend_hash_internal_pointer_reset_ex, zend_hash_move_forward_ex, zend_object, zend_set_timeout,
    zend_string, zend_unset_timeout, zval, zval_add_ref, zval_ptr_dtor,
};

mod headers;
mod receive;
mod request;
mod respond;
mod sendfile;
#[cfg(test)]
mod tests;

pub use sendfile::set_sendfile_root;

/// Live variants carry the Box pointer so paths where free_obj never runs (bailout) can still reclaim the unit.
#[derive(Clone, Copy)]
enum Unit {
    Idle,
    Handling(*mut ExchangeState),
    Sealed(*mut ExchangeState),
}

struct CycleState {
    unit: Unit,
    closed_seen: bool,
    served: bool,
    /// A unit was handed out this cycle: a fatal after that is an app failure, not a boot failure.
    received: bool,
    scope: Option<tracing::span::EnteredSpan>,
}

const CYCLE_IDLE: CycleState = CycleState {
    unit: Unit::Idle,
    closed_seen: false,
    served: false,
    received: false,
    scope: None,
};

thread_local! {
    static CYCLE: RefCell<CycleState> = const { RefCell::new(CYCLE_IDLE) };
}

fn update(f: impl FnOnce(&mut CycleState)) {
    CYCLE.with_borrow_mut(f);
}

fn current_unit() -> Unit {
    CYCLE.with_borrow(|c| c.unit)
}

/// # Safety
/// The returned context is owned by the active exchange on this PHP thread.
pub(crate) unsafe fn active_context<'a>() -> Option<&'a crate::types::Context> {
    let unit = CYCLE.with_borrow(|c| c.scope.as_ref().map(|_| c.unit));
    match unit {
        Some(Unit::Handling(ptr)) => Some(unsafe { &(*ptr).job.ctx }),
        _ => None,
    }
}

fn finish_execution(st: &mut ExchangeState, errored: bool) {
    let ptr = std::ptr::from_mut(st);
    update(|c| {
        if matches!(c.unit, Unit::Handling(p) | Unit::Sealed(p) if p == ptr) {
            c.scope = None;
        }
    });
    // A fatal can leave the exchange open for a shutdown response.
    let errored = errored || unsafe { (*crate::rapira_cg()).unclean_shutdown };
    st.job.ctx.finish_execution(errored);
}

/// A cycle that returned with an active exchange failed to complete that request.
pub(crate) fn finish_current() {
    if let Unit::Handling(ptr) = current_unit() {
        // SAFETY: the active unit remains owned until exchange_drop or reclaim_current.
        finish_execution(unsafe { &mut *ptr }, true);
    }
}

pub(crate) fn cycle_reset() {
    reclaim_current();
    update(|c| *c = CYCLE_IDLE);
}

/// Reclaim a unit free_obj never saw (shutdown bailout / allocation bailout).
pub(crate) fn reclaim_current() {
    if let Unit::Handling(ptr) | Unit::Sealed(ptr) = current_unit() {
        // SAFETY: the active unit remains owned until exchange_drop or this function.
        let st = unsafe { &mut *ptr };
        finish_execution(st, st.stage != Stage::Finalized);
        update(|c| c.unit = Unit::Idle);
        // SAFETY: the pointer came from Box::into_raw in finish_pull, and exchange_drop untracks before reclaiming.
        let st = unsafe { Box::from_raw(ptr) };
        if st.stage != Stage::Finalized {
            sb_update(Event::Handled(true));
        }
        drop(st);
    }
}

pub(crate) fn closed_seen() -> bool {
    CYCLE.with_borrow(|c| c.closed_seen)
}

pub(crate) fn note_closed() {
    update(|c| c.closed_seen = true);
}

pub(crate) fn note_received() {
    update(|c| c.received = true);
}

pub(crate) fn note_served() {
    update(|c| c.served = true);
}

pub(crate) fn served_any() -> bool {
    CYCLE.with_borrow(|c| c.served)
}

pub(crate) fn received_any() -> bool {
    CYCLE.with_borrow(|c| c.received)
}

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
    headers: FieldLines,
    body_coded: bool,
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

/// Rendered at construction so the builder frame holds no owned allocations (zend.rs frame rule).
enum AddrOwned {
    Inet {
        ip: String,
        port: u16,
    },
    /// None = unnamed endpoint.
    Unix(Option<Vec<u8>>),
}

fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

impl AddrOwned {
    fn new(a: &Addr) -> Self {
        match a {
            Addr::Inet(sa) => Self::Inet {
                ip: sa.ip().to_string(),
                port: sa.port(),
            },
            Addr::Unix(p) => Self::Unix(p.as_deref().map(path_bytes).filter(|b| !b.is_empty())),
        }
    }
}

/// Keys are CStrings: the symtable prefilter in add_assoc_zval_ex reads one byte past a leading `-`, which the terminator covers.
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

pub struct ExchangeState {
    // body above job: declaration drop order unlinks the spool files before the frame sender closes
    body: BodyState,
    job: Box<Job>,
    headers: Grouped,
    uri_abs: String,
    target: Vec<u8>,
    authority: Option<Vec<u8>>,
    /// Contract spelling for `Request::$protocol`: HTTP/2, not the CGI HTTP/2.0.
    protocol_php: String,
    remote: AddrOwned,
    server: AddrOwned,
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
    /// Err hands the job back with its sender intact; the caller fails the unit.
    fn new(mut job: Box<Job>) -> Result<Self, Box<Job>> {
        let taken = std::mem::replace(&mut job.ctx.req.body, Body::Raw(Box::new(std::io::empty())));
        let body = match taken {
            Body::Raw(mut reader) => {
                let mut buf = Vec::new();
                if let Err(e) = reader.read_to_end(&mut buf) {
                    tracing::error!(
                        target: "rapira",
                        "request body read failed for {} {}: {e}",
                        job.ctx.req.method, job.ctx.req.uri
                    );
                    job.ctx.req.body = Body::Raw(Box::new(std::io::empty()));
                    return Err(job);
                }
                BodyState::Raw(buf)
            }
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

        let req = &job.ctx.req;
        let headers = Grouped::new(&req.headers);
        let authority = req.authority.clone();
        let target = req
            .target
            .clone()
            .unwrap_or_else(|| req.uri.clone().into_bytes());
        let protocol_php = match req.protocol.as_str() {
            "HTTP/2.0" => "HTTP/2".to_owned(),
            "HTTP/3.0" => "HTTP/3".to_owned(),
            p => p.to_owned(),
        };
        let remote = AddrOwned::new(&req.remote);
        let server = AddrOwned::new(&req.server);

        let scheme = if req.https { "https" } else { "http" };
        let host = match &authority {
            Some(a) => String::from_utf8_lossy(a).into_owned(),
            None => match &req.server {
                Addr::Inet(sa) => sa.to_string(),
                Addr::Unix(_) => format!("{}:{}", req.server_name, req.server_port),
            },
        };
        let path = if req.uri.starts_with('/') {
            req.uri.as_str()
        } else {
            "/"
        };
        let uri_abs = format!("{scheme}://{host}{path}");

        let bodiless = req.method.eq_ignore_ascii_case("HEAD");

        Ok(Self {
            job,
            body,
            headers,
            uri_abs,
            target,
            authority,
            protocol_php,
            remote,
            server,
            stage: Stage::Open,
            head_sent: false,
            pending: None,
            declared_cl: None,
            sent_body: 0,
            discarded: false,
            bodiless,
            armed_at: Instant::now(),
        })
    }

    fn host_closed(&self) -> bool {
        self.discarded
            || (self.stage != Stage::Finalized
                && self.job.ctx.sender.as_ref().is_some_and(Sender::is_closed))
    }
}

/// Recovers the enclosing C struct: the C fields sit before `std` (wrapper.h layout).
unsafe fn exchange_from(obj: *mut zend_object) -> *mut rapira_exchange_obj {
    unsafe {
        obj.byte_sub(std::mem::offset_of!(rapira_exchange_obj, std))
            .cast()
    }
}

unsafe fn info_from(obj: *mut zend_object) -> *mut rapira_dispatcher_info_obj {
    unsafe {
        obj.byte_sub(std::mem::offset_of!(rapira_dispatcher_info_obj, std))
            .cast()
    }
}
