use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::{CStr, c_char, c_void},
    time::Instant,
};

use bytes::Bytes;
use tokio::sync::oneshot;

use super::{ErrorDetail, Job, Reply, Request, ServiceInfo, Status, metadata};
use crate::{
    callbacks::guard,
    exchange::AddrOwned,
    scoreboard::{Event, sb_update},
    start::{Pulled, pending_depth},
    types::FieldLines,
    work::{self, ReceiveWait, Work},
    zend, *,
};

thread_local! {
    pub(super) static SERVICES: RefCell<Option<Vec<ServiceInfo>>> = const { RefCell::new(None) };
    static CALLS: RefCell<HashMap<usize, Box<CallState>>> = RefCell::new(HashMap::new());
    static ACTIVE: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn install(services: Option<Vec<ServiceInfo>>) {
    SERVICES.with_borrow_mut(|slot| *slot = services);
}

pub(crate) fn installed() -> bool {
    SERVICES.with_borrow(Option::is_some)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    Finalized,
    Discarded,
}

pub(super) struct CallState {
    pub(super) request: Request,
    pub(super) remote: AddrOwned,
    pub(super) headers: FieldLines,
    pub(super) trailers: FieldLines,
    pub(super) owners: usize,
    sender: Option<oneshot::Sender<Reply>>,
    stage: Stage,
}

#[derive(Clone, Copy)]
enum Verb {
    Ok,
    Finalized,
    Discarded,
    Invalid(&'static CStr),
}

impl CallState {
    fn new(job: Job) -> Self {
        Self {
            remote: AddrOwned::new(&job.request.remote),
            request: job.request,
            headers: Vec::new(),
            trailers: Vec::new(),
            owners: 1,
            sender: Some(job.sender),
            stage: Stage::Open,
        }
    }

    fn cancelled(&self) -> bool {
        self.stage == Stage::Discarded
            || (self.stage == Stage::Open
                && (self.sender.as_ref().is_some_and(oneshot::Sender::is_closed)
                    || self
                        .request
                        .expires_at
                        .is_some_and(|end| Instant::now() >= end)))
    }

    fn finish_state(&mut self, stage: Stage, errored: bool, served: bool) {
        self.stage = stage;
        if ACTIVE.get() == self as *mut Self as usize {
            ACTIVE.set(0);
        }
        if served {
            work::note_served();
        }
        sb_update(Event::Handled(errored));
    }

    fn check(&mut self) -> Verb {
        if self.stage == Stage::Open && self.cancelled() {
            self.sender.take();
            self.finish_state(Stage::Discarded, true, false);
        }
        match self.stage {
            Stage::Open => Verb::Ok,
            Stage::Finalized => Verb::Finalized,
            Stage::Discarded => Verb::Discarded,
        }
    }

    fn complete(&mut self, result: Result<Bytes, Status>, served: bool) -> Verb {
        let state = self.check();
        if !matches!(state, Verb::Ok) {
            return state;
        }
        let errored = result.is_err();
        let reply = Reply {
            headers: self.headers.clone(),
            trailers: self.trailers.clone(),
            result,
        };
        let sent = self.sender.take().is_some_and(|tx| tx.send(reply).is_ok());
        self.finish_state(
            if sent {
                Stage::Finalized
            } else {
                Stage::Discarded
            },
            errored || !sent,
            served && sent,
        );
        if sent { Verb::Ok } else { Verb::Discarded }
    }

    fn abandon(&mut self) {
        if self.stage == Stage::Open {
            self.complete(
                Err(Status {
                    code: 13,
                    message: "call failed".into(),
                    details: Vec::new(),
                }),
                false,
            );
        }
    }
}

// This registry also owns states whose Zend free hook was skipped by a bailout.
pub(crate) fn reclaim() {
    let calls = CALLS.with_borrow_mut(std::mem::take);
    for (_, mut state) in calls {
        state.abandon();
    }
    ACTIVE.set(0);
}

pub(super) unsafe fn call_from(obj: *mut zend_object) -> *mut rapira_grpc_call_obj {
    unsafe {
        obj.byte_sub(std::mem::offset_of!(rapira_grpc_call_obj, std))
            .cast()
    }
}

pub(super) unsafe fn metadata_from(obj: *mut zend_object) -> *mut rapira_grpc_metadata_obj {
    unsafe {
        obj.byte_sub(std::mem::offset_of!(rapira_grpc_metadata_obj, std))
            .cast()
    }
}

unsafe fn outcome(verb: Verb) -> bool {
    unsafe {
        match verb {
            Verb::Ok => return true,
            Verb::Finalized => {
                zend::throw_exception(rapira_ce_already_finalized_error, c"the call already ended")
            }
            Verb::Discarded => zend::throw_exception(
                rapira_ce_work_discarded_exception,
                c"the host closed the call",
            ),
            Verb::Invalid(message) => zend::throw_value_error(message),
        }
    }
    false
}

/// # Safety
/// The engine is active and output is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_receive(timeout: i64, poll: bool, out: *mut zval) -> bool {
    guard(false, || unsafe {
        if timeout < -1 {
            zend_argument_value_error(1, c"must be greater than or equal to -1".as_ptr());
            return false;
        }
        let active = ACTIVE.get() as *mut CallState;
        if !active.is_null() && matches!((*active).check(), Verb::Ok) {
            zend::throw_error(
                c"receive() while a Rapira\\Grpc\\UnaryCall is unfinalized; finalize it first",
            );
            return false;
        }
        let mut object: zval = std::mem::zeroed();
        object_init_ex(&mut object, rapira_ce_internal_grpc_call);
        let wait = ReceiveWait::new(timeout);
        loop {
            rapira_receive_untimed();
            let pulled = wait.pull();
            rapira_receive_timed();
            match pulled {
                Pulled::Job(job) => {
                    let Work::Grpc(job) = job else {
                        job.unavailable();
                        continue;
                    };
                    let mut state = Box::new(CallState::new(*job));
                    let ptr = &raw mut *state;
                    CALLS.with_borrow_mut(|calls| {
                        calls.insert(ptr as usize, state);
                    });
                    (*call_from(object.value.obj)).state = ptr.cast();
                    ACTIVE.set(ptr as usize);
                    work::note_received();
                    *out = object;
                    return true;
                }
                Pulled::Closed => {
                    work::note_closed();
                    zval_ptr_dtor(&mut object);
                    zend::throw_exception(
                        rapira_ce_closed_exception,
                        c"no more work will ever arrive",
                    );
                    return false;
                }
                Pulled::Empty if poll => {
                    zval_ptr_dtor(&mut object);
                    zend::zval_null(out);
                    return true;
                }
                Pulled::Timeout | Pulled::Empty => {
                    zval_ptr_dtor(&mut object);
                    zend::throw_exception(
                        rapira_ce_timeout_exception,
                        c"no work became available within the timeout",
                    );
                    return false;
                }
            }
        }
    })
}

/// # Safety
/// Output is writable and the engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_info(out: *mut zval) {
    guard((), || unsafe {
        object_init_ex(out, rapira_ce_internal_grpc_dispatcher_info);
        let obj = (*out)
            .value
            .obj
            .byte_sub(std::mem::offset_of!(rapira_dispatcher_info_obj, std))
            .cast::<rapira_dispatcher_info_obj>();
        let active = ACTIVE.get() as *mut CallState;
        if !active.is_null() {
            (*active).check();
        }
        (*obj).pending = pending_depth() as i64;
        (*obj).active = i64::from(ACTIVE.get() != 0);
    });
}

/// # Safety
/// The pointer is null or belongs to a live native call or metadata object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_release(ptr: *mut c_void, call: bool) {
    guard((), || unsafe {
        if ptr.is_null() {
            return;
        }
        let state = &mut *ptr.cast::<CallState>();
        if call {
            state.abandon();
        }
        state.owners -= 1;
        if state.owners == 0 {
            let owned = CALLS.with_borrow_mut(|calls| calls.remove(&(ptr as usize)));
            drop(owned);
        }
    });
}

/// # Safety
/// The call is live and initialized. Output is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_response_metadata(
    call: *mut rapira_grpc_call_obj,
    out: *mut zval,
) {
    guard((), || unsafe {
        if zend::is_undef(&(*call).metadata) {
            object_init_ex(&raw mut (*call).metadata, rapira_ce_internal_grpc_metadata);
            let state = (*call).state.cast::<CallState>();
            (*state).owners += 1;
            (*metadata_from((*call).metadata.value.obj)).state = state.cast();
        }
        *out = (*call).metadata;
        zval_add_ref(out);
    });
}

/// # Safety
/// State is live. The returned bytes stay owned by the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_message(
    ptr: *mut c_void,
    len: *mut usize,
) -> *const c_char {
    unsafe {
        let bytes = &(*ptr.cast::<CallState>()).request.message;
        *len = bytes.len();
        if bytes.is_empty() {
            c"".as_ptr()
        } else {
            bytes.as_ptr().cast()
        }
    }
}

/// # Safety
/// State is a live native call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_cancelled(ptr: *mut c_void) -> bool {
    guard(false, || unsafe {
        matches!((*ptr.cast::<CallState>()).check(), Verb::Discarded)
    })
}

/// # Safety
/// State is a live native call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_finalized(ptr: *mut c_void) -> bool {
    guard(false, || unsafe {
        !matches!((*ptr.cast::<CallState>()).check(), Verb::Ok)
    })
}

/// # Safety
/// State is live and message is borrowed from ZPP.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_respond(
    ptr: *mut c_void,
    message: *mut zend_string,
) -> bool {
    guard(false, || unsafe {
        let result = (*ptr.cast::<CallState>())
            .complete(Ok(Bytes::copy_from_slice(zend::zstr_bytes(message))), true);
        outcome(result)
    })
}

// These final value classes have fixed declared property slots and no hooks.
unsafe fn status_value(obj: *mut zend_object) -> Result<Status, Verb> {
    unsafe {
        let props = (*obj).properties_table.as_ptr();
        if zend::zval_type(props) != IS_OBJECT
            || zend::zval_type(props.add(1)) != IS_STRING
            || zend::zval_type(props.add(2)) != IS_ARRAY
        {
            return Err(Verb::Invalid(c"Status must be initialized"));
        }
        let code = (*(*props).value.obj).properties_table.as_ptr().add(1);
        let table = (*props.add(2)).value.arr;
        let mut details = Vec::new();
        let mut pos = 0;
        zend_hash_internal_pointer_reset_ex(table, &mut pos);
        loop {
            let value = zend_hash_get_current_data_ex(table, &raw mut pos);
            if value.is_null() {
                break;
            }
            let fields = (*(*value).value.obj).properties_table.as_ptr();
            if zend::zval_type(fields) != IS_STRING || zend::zval_type(fields.add(1)) != IS_STRING {
                return Err(Verb::Invalid(c"ErrorDetail must be initialized"));
            }
            details.push(ErrorDetail {
                type_url: String::from_utf8_lossy(zend::zstr_bytes((*fields).value.str_))
                    .into_owned(),
                value: Bytes::copy_from_slice(zend::zstr_bytes((*fields.add(1)).value.str_)),
            });
            zend_hash_move_forward_ex(table, &mut pos);
        }
        Ok(Status {
            code: (*code).value.lval as u8,
            message: String::from_utf8_lossy(zend::zstr_bytes((*props.add(1)).value.str_))
                .into_owned(),
            details,
        })
    }
}

/// # Safety
/// State is live and status has the native Status type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_fail(ptr: *mut c_void, status: *mut zend_object) -> bool {
    guard(false, || unsafe {
        let result = match status_value(status) {
            Ok(status) => (*ptr.cast::<CallState>()).complete(Err(status), true),
            Err(error) => error,
        };
        outcome(result)
    })
}

fn reserved(name: &[u8]) -> bool {
    name.starts_with(b"grpc-")
        || name.starts_with(b"content-")
        || name.starts_with(b"connect-")
        || matches!(
            name,
            b"connection"
                | b"keep-alive"
                | b"proxy-connection"
                | b"transfer-encoding"
                | b"upgrade"
                | b"te"
                | b"trailer"
                | b"host"
                | b"accept-encoding"
                | b"user-agent"
        )
}

fn add_metadata(
    state: &mut CallState,
    name: &[u8],
    value: &[u8],
    binary: bool,
    trailer: bool,
) -> Verb {
    let stage = state.check();
    if !matches!(stage, Verb::Ok) {
        return stage;
    }
    let name = name.to_ascii_lowercase();
    if !metadata::valid_name(&name)
        || reserved(&name)
        || name.ends_with(b"-bin") != binary
        || (!binary && !metadata::valid_text(value))
    {
        return Verb::Invalid(c"invalid response metadata name, value, or binary suffix");
    }
    let name = String::from_utf8(name).expect("validated ASCII name");
    let fields = if trailer {
        &mut state.trailers
    } else {
        &mut state.headers
    };
    fields.push((name, value.to_vec()));
    Verb::Ok
}

/// # Safety
/// State is live. Name and value are borrowed from ZPP.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_add_metadata(
    ptr: *mut c_void,
    name: *mut zend_string,
    value: *mut zend_string,
    binary: bool,
    trailer: bool,
) -> bool {
    guard(false, || unsafe {
        let result = add_metadata(
            &mut *ptr.cast::<CallState>(),
            zend::zstr_bytes(name),
            zend::zstr_bytes(value),
            binary,
            trailer,
        );
        outcome(result)
    })
}
