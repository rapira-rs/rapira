//! Private PHP/Zend FFI adapter over `crate::call::CallState`, plus `ServiceInfo` and `Context`/`Metadata` Zval builders.

use std::ffi::{CStr, c_char};
use std::sync::OnceLock;

use buffa_descriptor::{DescriptorPool, MethodDescriptor};
use connectrpc::{ConnectError, ErrorCode, ErrorDetail, Protocol};
use rapira_sapi::callbacks::guard;
use rapira_sapi::exchange::{add_list, build_address};
use rapira_sapi::pool::{Event, record};
use rapira_sapi::stream;
use rapira_sapi::work::{Held, HeldPtr};
use rapira_sapi::{
    HashTable, IS_OBJECT, IS_STRING, add_next_index_object, object_init_ex, rapira_array_init,
    rapira_ce_already_finalized_error, rapira_ce_work_discarded_exception, rapira_zval_enum_case,
    zend, zend_argument_type_error, zend_class_entry, zend_long, zend_object, zend_string,
    zend_zval_value_name, zval,
};

use super::{
    rapira_ce_grpc_context, rapira_ce_grpc_error_detail,
    rapira_ce_grpc_headers_already_committed_error, rapira_ce_grpc_metadata,
    rapira_ce_grpc_method_info, rapira_ce_grpc_method_kind, rapira_ce_grpc_protocol,
    rapira_ce_grpc_server_streaming_call, rapira_ce_grpc_service_info, rapira_ce_grpc_unary_call,
};
use crate::call::{CallState, Handled, MetadataField, Verb, normalize_error_detail};

static METHOD_INFO_SLOTS: OnceLock<[u32; 4]> = OnceLock::new();
static SERVICE_INFO_SLOTS: OnceLock<[u32; 2]> = OnceLock::new();
static METADATA_SLOTS: OnceLock<[u32; 1]> = OnceLock::new();
static CONTEXT_SLOTS: OnceLock<[u32; 7]> = OnceLock::new();

fn record_handled(handled: Option<Handled>) {
    if let Some(outcome) = handled {
        record(Event::Handled {
            errored: outcome == Handled::Errored,
        });
    }
}

/// `Held` adapter pairing `CallState` with its PHP unit class entry and pool event reporting.
pub(crate) struct GrpcUnit(CallState);

impl GrpcUnit {
    pub(crate) fn new(state: CallState) -> Self {
        Self(state)
    }
}

impl Held for GrpcUnit {
    fn settled(&self) -> bool {
        self.0.settled()
    }

    fn cancelled(&self) -> bool {
        self.0.cancelled()
    }

    fn discard(&mut self) {
        record_handled(self.0.discard());
    }

    /// The drop of `CallState`'s reply reports the lost call as INTERNAL.
    fn lost(&mut self) {}

    fn shed(self: Box<Self>) {
        self.0.shed();
    }

    fn class(&self) -> *mut zend_class_entry {
        // SAFETY: MINIT sets the class entries before any worker starts.
        unsafe {
            if self.0.is_stream() {
                rapira_ce_grpc_server_streaming_call
            } else {
                rapira_ce_grpc_unary_call
            }
        }
    }
}

/// # Safety
/// `dst` is writable. The engine is active on this thread.
unsafe fn method_info(dst: *mut zval, pool: &DescriptorPool, m: &MethodDescriptor) {
    unsafe {
        let ce = rapira_ce_grpc_method_info;
        let [name_slot, input_slot, output_slot, kind_slot] =
            *METHOD_INFO_SLOTS.get_or_init(|| {
                [c"name", c"inputType", c"outputType", c"kind"].map(|n| zend::prop_offset(ce, n))
            });
        let _ = object_init_ex(dst, ce);
        let obj = (*dst).value.obj;
        zend::slot_stringl(obj, name_slot, m.name().as_bytes());
        let input = pool.message(m.input()).full_name();
        zend::slot_stringl(obj, input_slot, input.as_bytes());
        let output = pool.message(m.output()).full_name();
        zend::slot_stringl(obj, output_slot, output.as_bytes());
        let case = match (m.is_client_streaming(), m.is_server_streaming()) {
            (false, false) => c"Unary",
            (false, true) => c"ServerStreaming",
            (true, false) => c"ClientStreaming",
            (true, true) => c"BidiStreaming",
        };
        let mut kind: zval = std::mem::zeroed();
        rapira_zval_enum_case(&mut kind, rapira_ce_grpc_method_kind, case.as_ptr());
        zend::slot_init(obj, kind_slot, &kind);
    }
}

/// Implements `getServices()`. It builds a new `list<ServiceInfo>` each time.
/// # Safety
/// `rv` is writable. The engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_services(rv: *mut zval) -> bool {
    guard(false, || {
        crate::schema::with(|schema| unsafe {
            let ce = rapira_ce_grpc_service_info;
            let [name_slot, methods_slot] = *SERVICE_INFO_SLOTS
                .get_or_init(|| [c"name", c"methods"].map(|n| zend::prop_offset(ce, n)));
            let services = schema.services();
            rapira_array_init(rv, services.len() as u32);
            for s in services {
                let mut methods: zval = std::mem::zeroed();
                rapira_array_init(&mut methods, s.methods().len() as u32);
                for m in s.methods() {
                    let mut info: zval = std::mem::zeroed();
                    method_info(&mut info, schema.pool(), m);
                    let _ = add_next_index_object(&mut methods, info.value.obj);
                }
                let mut service: zval = std::mem::zeroed();
                let _ = object_init_ex(&mut service, ce);
                let obj = service.value.obj;
                zend::slot_stringl(obj, name_slot, s.full_name().as_bytes());
                zend::slot_init(obj, methods_slot, &methods);
                let _ = add_next_index_object(rv, obj);
            }
            true
        })
    })
}

/// Throws the PHP exception for `Verb` after owned Rust state has dropped.
/// # Safety
/// The engine is active. This function can bail out on OOM.
unsafe fn thrown(v: Verb) -> bool {
    unsafe {
        match v {
            Verb::Ok => return true,
            Verb::Finalized => zend::throw_exception(
                rapira_ce_already_finalized_error,
                c"the call was already finalized",
            ),
            Verb::Discarded => zend::throw_exception(
                rapira_ce_work_discarded_exception,
                c"the host closed the call first",
            ),
            Verb::HeadersCommitted => zend::throw_exception(
                rapira_ce_grpc_headers_already_committed_error,
                c"the first yield sent the headers",
            ),
            Verb::BadField(msg) => zend::throw_value_error(msg),
        }
    }
    false
}

/// Builds a `Rapira\Grpc\Metadata` object over `fields`.
/// # Safety
/// `dst` is writable. The engine is active on this thread.
unsafe fn build_metadata(dst: *mut zval, fields: &[MetadataField]) {
    unsafe {
        let mut entries: zval = std::mem::zeroed();
        rapira_array_init(&mut entries, fields.len() as u32);
        for MetadataField { name, values } in fields {
            add_list(
                &mut entries,
                name.as_str(),
                values.iter().map(Vec::as_slice),
            );
        }
        let ce = rapira_ce_grpc_metadata;
        let [entries_slot] = *METADATA_SLOTS.get_or_init(|| [zend::prop_offset(ce, c"entries")]);
        let _ = object_init_ex(dst, ce);
        zend::slot_init((*dst).value.obj, entries_slot, &entries);
    }
}

fn protocol_case(protocol: Protocol) -> &'static CStr {
    match protocol {
        Protocol::Grpc => c"Grpc",
        Protocol::GrpcWeb => c"GrpcWeb",
        _ => c"Connect",
    }
}

/// The listener does not terminate TLS, so `$tls` is null.
/// # Safety
/// `dst` is writable. The engine is active on this thread.
unsafe fn build_context(dst: *mut zval, st: &mut CallState) {
    unsafe {
        let mut metadata: zval = std::mem::zeroed();
        build_metadata(&mut metadata, st.context_metadata());
        let req = st.request();
        let mut remote: zval = std::mem::zeroed();
        build_address(&mut remote, &req.remote);
        let mut protocol: zval = std::mem::zeroed();
        rapira_zval_enum_case(
            &mut protocol,
            rapira_ce_grpc_protocol,
            protocol_case(req.protocol).as_ptr(),
        );

        let ce = rapira_ce_grpc_context;
        let [
            method_slot,
            metadata_slot,
            deadline_slot,
            remote_slot,
            tls_slot,
            protocol_slot,
            received_at_slot,
        ] = *CONTEXT_SLOTS.get_or_init(|| {
            [
                c"method",
                c"metadata",
                c"deadline",
                c"remote",
                c"tls",
                c"protocol",
                c"receivedAt",
            ]
            .map(|n| zend::prop_offset(ce, n))
        });
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::slot_stringl(o, method_slot, req.method.as_bytes());
        zend::slot_init(o, metadata_slot, &metadata);
        match req.deadline {
            Some(d) => zend::slot_double(o, deadline_slot, d.unix),
            None => zend::slot_null(o, deadline_slot),
        }
        zend::slot_init(o, remote_slot, &remote);
        zend::slot_null(o, tls_slot);
        zend::slot_init(o, protocol_slot, &protocol);
        zend::slot_double(o, received_at_slot, req.received_at);

        // The C shell memoizes Context once per call, so release the Rust copies now.
        st.release_context();
    }
}

/// Reads a string property of an `ErrorDetail`, returning `None` when not an initialized string.
/// # Safety
/// `obj` is a live `ErrorDetail`. The borrow must not outlive it.
unsafe fn detail_prop<'a>(obj: *mut zend_object, name: &CStr) -> Option<&'a [u8]> {
    unsafe {
        let mut rv: zval = std::mem::zeroed();
        let zv = zend::read_prop(rapira_ce_grpc_error_detail, obj, name, &mut rv);
        (zend::zval_type(zv) == IS_STRING).then(|| zend::zstr_bytes((*zv).value.str_))
    }
}

/// Converts `Status::$details` to `connectrpc` details, or returns the first item that is not an `ErrorDetail`.
/// # Safety
/// `details` is a live array.
unsafe fn read_details(
    details: *mut HashTable,
    protocol: Protocol,
) -> Result<Vec<ErrorDetail>, *mut zval> {
    unsafe {
        let mut out = Vec::new();
        for zend::Entry { val: item, .. } in zend::entries(details) {
            let pair = if zend::zval_type(item) == IS_OBJECT
                && zend::instanceof((*(*item).value.obj).ce, rapira_ce_grpc_error_detail)
            {
                let obj = (*item).value.obj;
                detail_prop(obj, c"typeUrl").zip(detail_prop(obj, c"value"))
            } else {
                None
            };
            let Some((url, value)) = pair else {
                return Err(item);
            };
            out.push(normalize_error_detail(protocol, url, value));
        }
        Ok(out)
    }
}

/// The state keeps the message; the C shell copies it once per call object.
/// # Safety
/// `held` is the slot of a live call. `len` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_message(
    held: *const HeldPtr,
    len: *mut usize,
) -> *const c_char {
    guard(c"".as_ptr(), || unsafe {
        let message = &(*(*held).state::<GrpcUnit>()).0.request().message;
        *len = message.len();
        zend::ptr_or_empty(message)
    })
}

/// Builds the Context into `memo`, the context memo of the call object. The C shell keeps the memo check.
/// # Safety
/// `held` is the held word of a live call that receive() attached. `memo` is writable and IS_UNDEF. The engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_context(held: *const HeldPtr, memo: *mut zval) -> bool {
    guard(false, || unsafe {
        let mut context: zval = std::mem::zeroed();
        build_context(&mut context, &mut (*(*held).state::<GrpcUnit>()).0);
        *memo = context;
        true
    })
}

/// # Safety
/// `held` is the slot of a live call. `message` points at `len` readable bytes. The engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_respond(
    held: *const HeldPtr,
    message: *const c_char,
    len: usize,
) -> bool {
    guard(false, || unsafe {
        let message = std::slice::from_raw_parts(message.cast::<u8>(), len);
        let (verb, handled) = (*(*held).state::<GrpcUnit>())
            .0
            .finish(Ok(message), |_, _, _| {
                unreachable!("unary calls never send stream parts")
            });
        record_handled(handled);
        thrown(verb)
    })
}

/// Starts `respond(\Generator)`. Returns false with an exception pending when the call takes no stream.
/// # Safety
/// `held` is the slot of a live call. The engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_stream_begin(held: *const HeldPtr) -> bool {
    guard(false, || unsafe {
        let (verb, handled) = (*(*held).state::<GrpcUnit>()).0.begin();
        record_handled(handled);
        thrown(verb)
    })
}

/// Sends one message that the generator of `respond(\Generator)` yielded. Throws nothing. Returns false when the host closed the call.
/// # Safety
/// `held` is the slot of a live call. `message` points at `len` readable bytes. The caller runs on a PHP thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_stream_send(
    held: *const HeldPtr,
    message: *const c_char,
    len: usize,
) -> bool {
    guard(false, || unsafe {
        let message = std::slice::from_raw_parts(message.cast::<u8>(), len);
        let (sent, handled) = (*(*held).state::<GrpcUnit>())
            .0
            .send(message, |tx, part, until| stream::send(tx, part, until));
        record_handled(handled);
        sent
    })
}

/// Runs when the generator of `respond(\Generator)` ends with the call open. Throws nothing.
/// # Safety
/// `held` is the slot of a live call. The caller runs on a PHP thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_stream_end(held: *const HeldPtr) {
    guard((), || unsafe {
        let handled = (*(*held).state::<GrpcUnit>())
            .0
            .end(|tx, part, until| stream::send(tx, part, until));
        record_handled(handled);
    });
}

/// Takes the three properties of a `Rapira\Grpc\Status`. `code` is the value of its StatusCode case. A detail that is not an ErrorDetail gives a TypeError before any state change.
/// # Safety
/// `held` is the slot of a live call. `message` and `details` are live. The engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_fail(
    held: *const HeldPtr,
    code: zend_long,
    message: *const zend_string,
    details: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        let st = &mut (*(*held).state::<GrpcUnit>()).0;
        let details = match read_details(details, st.request().protocol) {
            Ok(details) => details,
            Err(item) => {
                zend_argument_type_error(
                    1,
                    c"must hold only Rapira\\Grpc\\ErrorDetail details, %s given".as_ptr(),
                    zend_zval_value_name(item),
                );
                return false;
            }
        };
        let code = ErrorCode::from_grpc_code(code as u32).unwrap_or(ErrorCode::Unknown);
        let mut err = ConnectError::new(code, String::from_utf8_lossy(zend::zstr_bytes(message)));
        err.details = details;
        let (verb, handled) = st.finish(Err(err), |tx, part, until| stream::send(tx, part, until));
        record_handled(handled);
        thrown(verb)
    })
}

/// # Safety
/// `held` is the slot of a live call. `name` and `value` point at their readable bytes. The engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_add(
    held: *const HeldPtr,
    trailer: bool,
    binary: bool,
    name: *const c_char,
    name_len: usize,
    value: *const c_char,
    value_len: usize,
) -> bool {
    guard(false, || unsafe {
        let name = std::slice::from_raw_parts(name.cast::<u8>(), name_len);
        let value = std::slice::from_raw_parts(value.cast::<u8>(), value_len);
        let verb = (*(*held).state::<GrpcUnit>())
            .0
            .add(trailer, binary, name, value);
        thrown(verb)
    })
}

/// What one half holds so far, with raw `-bin` values.
/// # Safety
/// `held` is the slot of a live call. `rv` is writable. The engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_snapshot(
    held: *const HeldPtr,
    trailer: bool,
    rv: *mut zval,
) -> bool {
    guard(false, || unsafe {
        let st = &(*(*held).state::<GrpcUnit>()).0;
        build_metadata(rv, if trailer { st.trailers() } else { st.headers() });
        true
    })
}
