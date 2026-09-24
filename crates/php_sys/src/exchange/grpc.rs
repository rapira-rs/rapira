use std::cell::RefCell;

use base64::Engine as _;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD_NO_PAD};
use tokio::sync::oneshot;

use super::request::{add_list, build_address, header_key};
use super::respond::{Verb, throw_verb};
use super::*;
use crate::{
    IS_OBJECT, rapira_ce_grpc_context, rapira_ce_grpc_error_detail, rapira_ce_grpc_metadata,
    rapira_ce_grpc_method_info, rapira_ce_grpc_method_kind, rapira_ce_grpc_protocol,
    rapira_ce_grpc_service_info, rapira_ce_internal_grpc_response_metadata, rapira_grpc_call_obj,
    rapira_grpc_metadata_obj, rapira_zval_enum_case,
    types::{GrpcJob, GrpcMethod, GrpcOutcome, GrpcProtocol, GrpcRequest, GrpcService, GrpcStatus},
    zend_argument_type_error, zend_read_property, zend_zval_value_name,
};

thread_local! {
    /// Set on a PHP thread that serves a gRPC pool.
    static SERVICES: RefCell<Option<Vec<GrpcService>>> = const { RefCell::new(None) };
}

/// Makes this PHP thread serve a gRPC pool with `services`.
pub(crate) fn serve_grpc(services: Vec<GrpcService>) {
    SERVICES.set(Some(services));
}

pub(crate) fn serving() -> bool {
    SERVICES.with_borrow(Option::is_some)
}

/// The MethodKind case name: client streaming streams the request, server streaming streams the response.
fn kind_case(client_streaming: bool, server_streaming: bool) -> &'static CStr {
    match (client_streaming, server_streaming) {
        (false, false) => c"Unary",
        (false, true) => c"ServerStreaming",
        (true, false) => c"ClientStreaming",
        (true, true) => c"BidiStreaming",
    }
}

/// # Safety
/// `dst` writable; engine active on this thread.
unsafe fn method_info(dst: *mut zval, m: &GrpcMethod) {
    unsafe {
        let ce = rapira_ce_grpc_method_info;
        let _ = object_init_ex(dst, ce);
        let obj = (*dst).value.obj;
        zend::prop_stringl(ce, obj, c"name", m.name.as_bytes());
        zend::prop_stringl(ce, obj, c"inputType", m.input_type.as_bytes());
        zend::prop_stringl(ce, obj, c"outputType", m.output_type.as_bytes());
        let mut kind: zval = std::mem::zeroed();
        rapira_zval_enum_case(
            &mut kind,
            rapira_ce_grpc_method_kind,
            kind_case(m.client_streaming, m.server_streaming).as_ptr(),
        );
        zend::prop_zval(ce, obj, c"kind", &mut kind);
        zval_ptr_dtor(&mut kind);
    }
}

/// `getServices()`: a new `list<ServiceInfo>` on each call.
/// # Safety
/// `rv` writable; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_services(rv: *mut zval) -> bool {
    guard(false, || unsafe {
        SERVICES.with_borrow(|services| {
            let services = services.as_deref().unwrap_or_default();
            rapira_array_init(rv, services.len() as u32);
            for s in services {
                let mut methods: zval = std::mem::zeroed();
                rapira_array_init(&mut methods, s.methods.len() as u32);
                for m in &s.methods {
                    let mut info: zval = std::mem::zeroed();
                    method_info(&mut info, m);
                    let _ = add_next_index_object(&mut methods, info.value.obj);
                }
                let ce = rapira_ce_grpc_service_info;
                let mut service: zval = std::mem::zeroed();
                let _ = object_init_ex(&mut service, ce);
                zend::prop_stringl(ce, service.value.obj, c"name", s.name.as_bytes());
                zend::prop_zval(ce, service.value.obj, c"methods", &mut methods);
                zval_ptr_dtor(&mut methods);
                let _ = add_next_index_object(rv, service.value.obj);
            }
        });
        true
    })
}

/// One metadata name with its values in arrival order (request) or add order (response). `-bin` values are raw bytes.
type Field = (HeaderName, Vec<Vec<u8>>);
/// In order of the first value of each name.
type Fields = Vec<Field>;

/// Request `-bin` values use the standard alphabet with or without padding. Implementations "MUST accept padded and un-padded values": https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
const BIN_DECODE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Names that the transport owns: gRPC and Connect control fields, content framing, Connect trailers in the head, and the HTTP connection-specific fields. `name` is lower-case.
fn reserved(name: &str) -> bool {
    const PREFIXES: [&str; 4] = ["grpc-", "connect-", "content-", "trailer-"];
    const NAMES: [&str; 9] = [
        "te",
        "trailer",
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
        "host",
        "accept-encoding",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p)) || NAMES.contains(&name)
}

/// `Context::$metadata`: the application keys, with `-bin` values decoded. An undecodable value is dropped, and a key with no values left is absent.
fn context_metadata(headers: &HeaderMap) -> Fields {
    let mut out = Fields::new();
    for name in headers.keys() {
        if reserved(name.as_str()) {
            continue;
        }
        let binary = name.as_str().ends_with("-bin");
        let values: Vec<Vec<u8>> = headers
            .get_all(name)
            .iter()
            .filter_map(|v| {
                if !binary {
                    return Some(v.as_bytes().to_vec());
                }
                BIN_DECODE
                    .decode(v.as_bytes())
                    .inspect_err(|e| {
                        tracing::debug!(target: "rapira", "dropped an undecodable {name} value: {e}");
                    })
                    .ok()
            })
            .collect();
        if !values.is_empty() {
            out.push((name.clone(), values));
        }
    }
    out
}

/// Response metadata in wire form. `-bin` values are base64 without padding: implementations "should emit un-padded values" (PROTOCOL-HTTP2 Requests).
fn wire(fields: &[Field]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, values) in fields {
        let binary = name.as_str().ends_with("-bin");
        for v in values {
            let value = if binary {
                HeaderValue::try_from(STANDARD_NO_PAD.encode(v))
            } else {
                HeaderValue::from_bytes(v)
            };
            // add_core accepted only printable ASCII text values
            if let Ok(value) = value {
                map.append(name.clone(), value);
            }
        }
    }
    map
}

/// Owned values of the Context object. The state keeps them because a Zend OOM bailout longjmps through the builder frame, so that frame holds no values with Drop glue.
struct ContextView {
    remote: AddrOwned,
    metadata: Fields,
}

pub(crate) struct GrpcState {
    req: GrpcRequest,
    received_at: f64,
    /// None after the outcome went out or the call was discarded.
    reply: Option<oneshot::Sender<GrpcOutcome>>,
    finalized: bool,
    discarded: bool,
    headers: Fields,
    trailers: Fields,
    /// Filled by the first `getContext()`.
    view: Option<ContextView>,
}

impl GrpcState {
    pub(super) fn new(job: Box<GrpcJob>) -> Self {
        let GrpcJob {
            req,
            received_at,
            reply,
        } = *job;
        Self {
            req,
            received_at,
            reply: Some(reply),
            finalized: false,
            discarded: false,
            headers: Fields::new(),
            trailers: Fields::new(),
            view: None,
        }
    }

    /// Work::isFinalized(): the worker committed the outcome, or the host closed the call.
    fn is_finalized(&self) -> bool {
        self.finalized || self.host_closed()
    }

    /// Work::isCancelled(): the host no longer accepts an outcome.
    fn is_cancelled(&self) -> bool {
        self.host_closed()
    }
}

impl Held for GrpcState {
    fn finalized(&self) -> bool {
        self.finalized
    }

    /// The host dropped the receiver: the deadline passed or the client left.
    fn host_closed(&self) -> bool {
        self.discarded
            || (!self.finalized && self.reply.as_ref().is_some_and(oneshot::Sender::is_closed))
    }

    /// Setting `finalized` keeps `rapira_rs_grpc_drop`/`reclaim_current` from counting the call a second time.
    fn discard(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.discarded = true;
        self.reply = None;
        sb_update(Event::Handled(true));
    }
}

/// Validates one response metadata entry, then adds it to its half. The entry is checked before the state, so a bad entry gives a ValueError on a finalized call too.
fn add_core(
    st: Option<&mut GrpcState>,
    trailer: bool,
    binary: bool,
    name: &[u8],
    value: &[u8],
) -> Verb {
    // from_bytes lower-cases the name
    let Ok(name) = HeaderName::from_bytes(name) else {
        return Verb::BadField(c"the metadata name is not a valid header name");
    };
    if reserved(name.as_str()) {
        return Verb::BadField(c"the metadata name is reserved for the transport");
    }
    match (binary, name.as_str().ends_with("-bin")) {
        (false, true) => {
            return Verb::BadField(
                c"a metadata name with the -bin suffix takes binary values: use addBinaryHeader() or addBinaryTrailer()",
            );
        }
        (true, false) => {
            return Verb::BadField(c"a binary metadata name must have the -bin suffix");
        }
        _ => {}
    }
    if !binary && !value.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return Verb::BadField(c"a text metadata value must be printable ASCII");
    }
    let Some(st) = st else {
        return Verb::Finalized;
    };
    if st.is_finalized() {
        return Verb::Finalized;
    }
    let half = if trailer {
        &mut st.trailers
    } else {
        &mut st.headers
    };
    match half.iter_mut().find(|(n, _)| *n == name) {
        Some((_, values)) => values.push(value.to_vec()),
        None => half.push((name, vec![value.to_vec()])),
    }
    Verb::Ok
}

/// Sends the one outcome of the call, with both metadata halves in wire form.
fn finish(st: &mut GrpcState, result: Result<Bytes, GrpcStatus>) -> Verb {
    if st.host_closed() {
        st.discard();
        return Verb::Discarded;
    }
    if st.finalized {
        return Verb::Finalized;
    }
    st.finalized = true;
    let outcome = GrpcOutcome {
        headers: wire(&st.headers),
        trailers: wire(&st.trailers),
        result,
    };
    // the receiver can drop after the host_closed() check
    if st
        .reply
        .take()
        .is_some_and(|reply| reply.send(outcome).is_ok())
    {
        note_served();
        sb_update(Event::Handled(false));
        Verb::Ok
    } else {
        st.discarded = true;
        sb_update(Event::Handled(true));
        Verb::Discarded
    }
}

/// # Safety
/// Engine active; can bailout on OOM.
unsafe fn thrown(v: Verb) -> bool {
    match v {
        Verb::Ok => true,
        v => {
            unsafe { throw_verb(v) };
            false
        }
    }
}

/// Recovers the enclosing C struct: the C fields sit before `std` (wrapper.h layout).
pub(super) unsafe fn grpc_call_from(obj: *mut zend_object) -> *mut rapira_grpc_call_obj {
    unsafe {
        obj.byte_sub(std::mem::offset_of!(rapira_grpc_call_obj, std))
            .cast()
    }
}

unsafe fn metadata_from(obj: *mut zend_object) -> *mut rapira_grpc_metadata_obj {
    unsafe {
        obj.byte_sub(std::mem::offset_of!(rapira_grpc_metadata_obj, std))
            .cast()
    }
}

/// A `Rapira\Grpc\Metadata` object over `fields`.
/// # Safety
/// `dst` writable; engine active on this thread.
unsafe fn build_metadata(dst: *mut zval, fields: &[Field]) {
    unsafe {
        let mut entries: zval = std::mem::zeroed();
        rapira_array_init(&mut entries, fields.len() as u32);
        for (name, values) in fields {
            let key = name.as_str();
            add_list(
                &mut entries,
                header_key(key),
                key.len(),
                values.iter().map(Vec::as_slice),
            );
        }
        let ce = rapira_ce_grpc_metadata;
        let _ = object_init_ex(dst, ce);
        zend::prop_zval(ce, (*dst).value.obj, c"entries", &mut entries);
        zval_ptr_dtor(&mut entries);
    }
}

fn protocol_case(protocol: GrpcProtocol) -> &'static CStr {
    match protocol {
        GrpcProtocol::Grpc => c"Grpc",
        GrpcProtocol::GrpcWeb => c"GrpcWeb",
        GrpcProtocol::Connect => c"Connect",
    }
}

/// The listener does not terminate TLS, so `$tls` is null.
/// # Safety
/// `dst` writable; engine active on this thread.
unsafe fn build_context(dst: *mut zval, st: &mut GrpcState) {
    unsafe {
        let req = &st.req;
        let view = st.view.get_or_insert_with(|| ContextView {
            remote: AddrOwned::new(&req.remote),
            metadata: context_metadata(&req.metadata),
        });
        let mut metadata: zval = std::mem::zeroed();
        build_metadata(&mut metadata, &view.metadata);
        let mut remote: zval = std::mem::zeroed();
        build_address(&mut remote, &view.remote);
        let mut protocol: zval = std::mem::zeroed();
        rapira_zval_enum_case(
            &mut protocol,
            rapira_ce_grpc_protocol,
            protocol_case(req.protocol).as_ptr(),
        );

        let ce = rapira_ce_grpc_context;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_stringl(ce, o, c"method", req.method.as_bytes());
        zend::prop_zval(ce, o, c"metadata", &mut metadata);
        zval_ptr_dtor(&mut metadata);
        match req.deadline {
            Some(d) => zend::prop_double(ce, o, c"deadline", d),
            None => zend::prop_null(ce, o, c"deadline"),
        }
        zend::prop_zval(ce, o, c"remote", &mut remote);
        zval_ptr_dtor(&mut remote);
        zend::prop_null(ce, o, c"tls");
        zend::prop_zval(ce, o, c"protocol", &mut protocol);
        zval_ptr_dtor(&mut protocol);
        zend::prop_double(ce, o, c"receivedAt", st.received_at);
    }
}

/// A string property of an ErrorDetail. None when the property is not an initialized string.
/// # Safety
/// `obj` a live ErrorDetail; the borrow must not outlive it.
unsafe fn detail_prop<'a>(obj: *mut zend_object, name: &CStr) -> Option<&'a [u8]> {
    unsafe {
        let mut rv: zval = std::mem::zeroed();
        let zv = zend_read_property(
            rapira_ce_grpc_error_detail,
            obj,
            name.as_ptr(),
            name.count_bytes(),
            true,
            &mut rv,
        );
        (zend::zval_type(zv) == IS_STRING).then(|| zend::zstr_bytes((*zv).value.str_))
    }
}

/// `Status::$details` as owned pairs, or the first item that is not an ErrorDetail.
/// `&raw mut pos`: the pos parameter is *mut on PHP 8.4 and *const on 8.5.
/// # Safety
/// `details` a live array.
unsafe fn read_details(details: *mut HashTable) -> Result<Vec<(String, Bytes)>, *mut zval> {
    unsafe {
        let mut out = Vec::new();
        let mut pos: HashPosition = 0;
        zend_hash_internal_pointer_reset_ex(details, &mut pos);
        loop {
            let item = zend_hash_get_current_data_ex(details, &raw mut pos);
            if item.is_null() {
                return Ok(out);
            }
            let item = zend::deref(item);
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
            out.push((
                String::from_utf8_lossy(url).into_owned(),
                Bytes::copy_from_slice(value),
            ));
            zend_hash_move_forward_ex(details, &mut pos);
        }
    }
}

/// # Safety
/// `state` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_is_finalized(state: *const c_void) -> bool {
    guard(false, || unsafe {
        (*state.cast::<GrpcState>()).is_finalized()
    })
}

/// # Safety
/// `state` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_is_cancelled(state: *const c_void) -> bool {
    guard(false, || unsafe {
        (*state.cast::<GrpcState>()).is_cancelled()
    })
}

/// The state keeps the message; the C shell copies it.
/// # Safety
/// `state` from receive; `len` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_message(
    state: *const c_void,
    len: *mut usize,
) -> *const c_char {
    guard(c"".as_ptr(), || unsafe {
        let message = &(*state.cast::<GrpcState>()).req.message;
        *len = message.len();
        zend::ptr_or_empty(message)
    })
}

/// Builds the Context on the first call and returns the same object after that.
/// # Safety
/// `call` a live call object with a state; `rv` writable; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_context(
    call: *mut rapira_grpc_call_obj,
    rv: *mut zval,
) -> bool {
    guard(false, || unsafe {
        if zend::is_undef(&(*call).context) {
            build_context(
                &mut (*call).context,
                &mut *(*call).state.cast::<GrpcState>(),
            );
        }
        *rv = (*call).context;
        zval_add_ref(rv);
        true
    })
}

/// Creates the accumulator on the first call and returns the same object after that. It borrows the state of the call.
/// # Safety
/// As `rapira_rs_grpc_context`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_response_metadata(
    call: *mut rapira_grpc_call_obj,
    rv: *mut zval,
) -> bool {
    guard(false, || unsafe {
        if zend::is_undef(&(*call).metadata) {
            let _ = object_init_ex(
                &mut (*call).metadata,
                rapira_ce_internal_grpc_response_metadata,
            );
            (*metadata_from((*call).metadata.value.obj)).state = (*call).state;
        }
        *rv = (*call).metadata;
        zval_add_ref(rv);
        true
    })
}

/// # Safety
/// `state` from receive; `message` points at `len` readable bytes; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_respond(
    state: *mut c_void,
    message: *const c_char,
    len: usize,
) -> bool {
    guard(false, || unsafe {
        let message = Bytes::copy_from_slice(std::slice::from_raw_parts(message.cast::<u8>(), len));
        thrown(finish(&mut *state.cast::<GrpcState>(), Ok(message)))
    })
}

/// `code` is the backing value of a StatusCode case. A detail that is not an ErrorDetail gives a TypeError before any state change.
/// # Safety
/// `state` from receive; `message` points at `message_len` readable bytes; `details` a live array; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_fail(
    state: *mut c_void,
    code: i64,
    message: *const c_char,
    message_len: usize,
    details: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        let details = match read_details(details) {
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
        let message = std::slice::from_raw_parts(message.cast::<u8>(), message_len);
        let status = GrpcStatus {
            code: code as u32,
            message: String::from_utf8_lossy(message).into_owned(),
            details,
        };
        thrown(finish(&mut *state.cast::<GrpcState>(), Err(status)))
    })
}

/// # Safety
/// `state` NULL (the call object is gone) or from receive; `name` and `value` point at their readable bytes; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_add(
    state: *mut c_void,
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
        thrown(add_core(
            state.cast::<GrpcState>().as_mut(),
            trailer,
            binary,
            name,
            value,
        ))
    })
}

/// What one half holds so far, with raw `-bin` values. Empty when the call object is gone.
/// # Safety
/// `state` NULL or from receive; `rv` writable; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_snapshot(
    state: *const c_void,
    trailer: bool,
    rv: *mut zval,
) -> bool {
    guard(false, || unsafe {
        let fields: &[_] = match state.cast::<GrpcState>().as_ref() {
            Some(st) if trailer => &st.trailers,
            Some(st) => &st.headers,
            None => &[],
        };
        build_metadata(rv, fields);
        true
    })
}

/// Reclaims the Box on free_obj. An unfinalized call drops its reply sender, and the host reports the call as lost.
/// # Safety
/// `state` a non-null pointer from `Box::into_raw` in receive; free_obj checks for NULL before the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_drop(state: *mut c_void) {
    guard((), || {
        let ptr: *mut GrpcState = state.cast();
        update(|c| {
            if c.unit.is_some_and(|u| std::ptr::addr_eq(u, ptr)) {
                c.unit = None;
            }
        });
        let st = unsafe { Box::from_raw(ptr) };
        if !st.finalized {
            sb_update(Event::Handled(true));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        client_streaming: bool,
        server_streaming: bool,
        expected: &'static CStr,
    }

    // Expected values: the MethodKind cases of the Rapira\Grpc contract and the two streaming flags of a protobuf MethodDescriptorProto.
    const CASES: &[Case] = &[
        Case {
            name: "no streaming is unary",
            client_streaming: false,
            server_streaming: false,
            expected: c"Unary",
        },
        Case {
            name: "a streamed response is server streaming",
            client_streaming: false,
            server_streaming: true,
            expected: c"ServerStreaming",
        },
        Case {
            name: "a streamed request is client streaming",
            client_streaming: true,
            server_streaming: false,
            expected: c"ClientStreaming",
        },
        Case {
            name: "both streamed is bidi streaming",
            client_streaming: true,
            server_streaming: true,
            expected: c"BidiStreaming",
        },
    ];

    #[test]
    fn method_kind_follows_the_streaming_flags() {
        for c in CASES {
            assert_eq!(
                kind_case(c.client_streaming, c.server_streaming),
                c.expected,
                "{}",
                c.name
            );
        }
    }

    fn state() -> (GrpcState, oneshot::Receiver<GrpcOutcome>) {
        let (reply, rx) = oneshot::channel();
        let req = GrpcRequest {
            method: "rapira.test.v1.EchoService/Echo".into(),
            protocol: GrpcProtocol::Grpc,
            metadata: HeaderMap::new(),
            deadline: None,
            remote: Addr::Inet(([127, 0, 0, 1], 50051).into()),
            message: Bytes::new(),
        };
        let job = Box::new(GrpcJob {
            req,
            received_at: 0.0,
            reply,
        });
        (GrpcState::new(job), rx)
    }

    fn map(lines: &[(&'static str, &'static str)]) -> HeaderMap {
        lines
            .iter()
            .map(|&(k, v)| (HeaderName::from_static(k), HeaderValue::from_static(v)))
            .collect()
    }

    fn owned(fields: &[(&str, &[&[u8]])]) -> Vec<(String, Vec<Vec<u8>>)> {
        fields
            .iter()
            .map(|(n, vs)| ((*n).to_owned(), vs.iter().map(|v| v.to_vec()).collect()))
            .collect()
    }

    fn named(fields: &[Field]) -> Vec<(String, Vec<Vec<u8>>)> {
        fields
            .iter()
            .map(|(n, vs)| (n.as_str().to_owned(), vs.clone()))
            .collect()
    }

    #[test]
    fn reserved_names_cover_the_transport() {
        struct Case {
            name: &'static str,
            header: &'static str,
            reserved: bool,
        }
        // Expected values: the approved reserved set. Prefixes grpc-, connect-, content-, trailer-, and the HTTP connection-specific names.
        const CASES: &[Case] = &[
            Case {
                name: "grpc- prefix",
                header: "grpc-timeout",
                reserved: true,
            },
            Case {
                name: "grpc- prefix on a -bin name",
                header: "grpc-status-details-bin",
                reserved: true,
            },
            Case {
                name: "content- prefix",
                header: "content-type",
                reserved: true,
            },
            Case {
                name: "connect- prefix",
                header: "connect-timeout-ms",
                reserved: true,
            },
            Case {
                name: "trailer- prefix",
                header: "trailer-x",
                reserved: true,
            },
            Case {
                name: "te",
                header: "te",
                reserved: true,
            },
            Case {
                name: "trailer",
                header: "trailer",
                reserved: true,
            },
            Case {
                name: "host",
                header: "host",
                reserved: true,
            },
            Case {
                name: "connection",
                header: "connection",
                reserved: true,
            },
            Case {
                name: "keep-alive",
                header: "keep-alive",
                reserved: true,
            },
            Case {
                name: "proxy-connection",
                header: "proxy-connection",
                reserved: true,
            },
            Case {
                name: "transfer-encoding",
                header: "transfer-encoding",
                reserved: true,
            },
            Case {
                name: "upgrade",
                header: "upgrade",
                reserved: true,
            },
            Case {
                name: "accept-encoding",
                header: "accept-encoding",
                reserved: true,
            },
            Case {
                name: "an application name",
                header: "user-agent",
                reserved: false,
            },
            Case {
                name: "an x- name",
                header: "x-user",
                reserved: false,
            },
            Case {
                name: "authorization is the application's",
                header: "authorization",
                reserved: false,
            },
            Case {
                name: "grpc without the dash",
                header: "grpcx",
                reserved: false,
            },
            Case {
                name: "grpc inside the name",
                header: "x-grpc-web",
                reserved: false,
            },
        ];
        for c in CASES {
            assert_eq!(reserved(c.header), c.reserved, "{}", c.name);
        }
    }

    #[test]
    fn context_metadata_filters_and_decodes() {
        struct Case {
            name: &'static str,
            headers: &'static [(&'static str, &'static str)],
            expected: &'static [(&'static str, &'static [&'static [u8]])],
        }
        // Expected values: Call/Context.php (application keys only), PROTOCOL-HTTP2 Requests (accept padded and unpadded -bin values) and RFC 4648 section 4 (`AP8` is 00 ff).
        const CASES: &[Case] = &[
            Case {
                name: "application key kept",
                headers: &[("user-agent", "t/1")],
                expected: &[("user-agent", &[b"t/1"])],
            },
            Case {
                name: "repeats keep order",
                headers: &[("x-user", "a"), ("x-user", "b")],
                expected: &[("x-user", &[b"a", b"b"])],
            },
            Case {
                name: "unpadded -bin",
                headers: &[("x-trace-bin", "AP8")],
                expected: &[("x-trace-bin", &[b"\x00\xff"])],
            },
            Case {
                name: "padded -bin",
                headers: &[("x-pad-bin", "AP8=")],
                expected: &[("x-pad-bin", &[b"\x00\xff"])],
            },
            Case {
                name: "undecodable -bin dropped",
                headers: &[("x-bad-bin", "!!")],
                expected: &[],
            },
            Case {
                name: "grpc- dropped",
                headers: &[("grpc-timeout", "1S")],
                expected: &[],
            },
            Case {
                name: "content- dropped",
                headers: &[("content-type", "application/grpc")],
                expected: &[],
            },
            Case {
                name: "connect- dropped",
                headers: &[("connect-protocol-version", "1")],
                expected: &[],
            },
            Case {
                name: "trailer- dropped",
                headers: &[("trailer-x", "y")],
                expected: &[],
            },
            Case {
                name: "connection-specific dropped",
                headers: &[("te", "trailers"), ("connection", "close"), ("host", "e")],
                expected: &[],
            },
        ];
        for c in CASES {
            assert_eq!(
                named(&context_metadata(&map(c.headers))),
                owned(c.expected),
                "{}",
                c.name
            );
        }
    }

    #[derive(Clone, Copy)]
    enum Setup {
        Open,
        Responded,
        HostClosed,
        Detached,
    }

    #[derive(Debug)]
    enum Want {
        Stored(&'static str, &'static [u8]),
        Bad,
        Finalized,
    }

    #[test]
    fn add_validates_per_contract() {
        struct Case {
            name: &'static str,
            setup: Setup,
            trailer: bool,
            binary: bool,
            key: &'static [u8],
            value: &'static [u8],
            want: Want,
        }
        // Expected values: ResponseMetadata.php (lower-case names, reserved names, the -bin rule, AlreadyFinalizedError), the approved printable ASCII rule for text values (empty allowed), and the HTTP error order (the ValueError first).
        const CASES: &[Case] = &[
            Case {
                name: "text header lower-cases",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"X-Cost",
                value: b"2.7",
                want: Want::Stored("x-cost", b"2.7"),
            },
            Case {
                name: "binary header keeps raw bytes",
                setup: Setup::Open,
                trailer: false,
                binary: true,
                key: b"x-tok-bin",
                value: b"\x00\xff",
                want: Want::Stored("x-tok-bin", b"\x00\xff"),
            },
            Case {
                name: "text trailer goes to the trailer half",
                setup: Setup::Open,
                trailer: true,
                binary: false,
                key: b"x-t",
                value: b"w",
                want: Want::Stored("x-t", b"w"),
            },
            Case {
                name: "grpc prefix reserved",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"grpc-status",
                value: b"0",
                want: Want::Bad,
            },
            Case {
                name: "content prefix reserved",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"content-type",
                value: b"x",
                want: Want::Bad,
            },
            Case {
                name: "connect prefix reserved",
                setup: Setup::Open,
                trailer: true,
                binary: false,
                key: b"connect-timeout-ms",
                value: b"1",
                want: Want::Bad,
            },
            Case {
                name: "trailer prefix reserved",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"trailer-x",
                value: b"1",
                want: Want::Bad,
            },
            Case {
                name: "connection reserved",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"connection",
                value: b"close",
                want: Want::Bad,
            },
            Case {
                name: "text method refuses -bin",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"x-tok-bin",
                value: b"a",
                want: Want::Bad,
            },
            Case {
                name: "binary method needs -bin",
                setup: Setup::Open,
                trailer: true,
                binary: true,
                key: b"x-tok",
                value: b"a",
                want: Want::Bad,
            },
            Case {
                name: "non-ASCII text value",
                setup: Setup::Open,
                trailer: true,
                binary: false,
                key: b"x-a",
                value: "é".as_bytes(),
                want: Want::Bad,
            },
            Case {
                name: "control byte in text value",
                setup: Setup::Open,
                trailer: true,
                binary: false,
                key: b"x-a",
                value: b"a\nb",
                want: Want::Bad,
            },
            Case {
                name: "empty text value",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"x-a",
                value: b"",
                want: Want::Stored("x-a", b""),
            },
            Case {
                name: "empty name",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"",
                value: b"a",
                want: Want::Bad,
            },
            Case {
                name: "bad name wins over finalized",
                setup: Setup::Responded,
                trailer: false,
                binary: false,
                key: b"grpc-x",
                value: b"a",
                want: Want::Bad,
            },
            Case {
                name: "after finalize",
                setup: Setup::Responded,
                trailer: false,
                binary: false,
                key: b"x-a",
                value: b"a",
                want: Want::Finalized,
            },
            Case {
                name: "after host close",
                setup: Setup::HostClosed,
                trailer: true,
                binary: false,
                key: b"x-a",
                value: b"a",
                want: Want::Finalized,
            },
            Case {
                name: "detached accumulator",
                setup: Setup::Detached,
                trailer: false,
                binary: false,
                key: b"x-a",
                value: b"a",
                want: Want::Finalized,
            },
        ];
        for c in CASES {
            let (mut st, rx) = state();
            let _rx = match c.setup {
                Setup::Responded => {
                    assert_eq!(finish(&mut st, Ok(Bytes::new())), Verb::Ok, "{}", c.name);
                    Some(rx)
                }
                Setup::HostClosed => {
                    drop(rx);
                    None
                }
                Setup::Open | Setup::Detached => Some(rx),
            };
            let target = match c.setup {
                Setup::Detached => None,
                _ => Some(&mut st),
            };
            let v = add_core(target, c.trailer, c.binary, c.key, c.value);
            match c.want {
                Want::Stored(name, value) => {
                    assert_eq!(v, Verb::Ok, "{}", c.name);
                    let half = if c.trailer { &st.trailers } else { &st.headers };
                    assert_eq!(named(half), owned(&[(name, &[value])]), "{}", c.name);
                }
                Want::Bad => assert!(matches!(v, Verb::BadField(_)), "{}: {v:?}", c.name),
                Want::Finalized => assert_eq!(v, Verb::Finalized, "{}", c.name),
            }
        }
    }

    /// A status triple: the code, the message and the (type URL, value) details.
    type Triple = (u32, &'static str, &'static [(&'static str, &'static [u8])]);

    enum Finish {
        Respond(&'static [u8]),
        Fail(Triple),
    }

    enum Sent {
        /// The receiver gets this outcome: headers, trailers (wire form) and the result.
        Outcome {
            headers: &'static [(&'static str, &'static str)],
            trailers: &'static [(&'static str, &'static str)],
            result: Result<&'static [u8], Triple>,
        },
        /// The state is dropped unfinalized: the receiver sees a closed channel.
        Lost,
        /// The test dropped the receiver.
        Nothing,
    }

    fn status(code: u32, message: &str, details: &[(&str, &[u8])]) -> GrpcStatus {
        GrpcStatus {
            code,
            message: message.to_owned(),
            details: details
                .iter()
                .map(|&(url, value)| (url.to_owned(), Bytes::copy_from_slice(value)))
                .collect(),
        }
    }

    #[test]
    fn finish_sends_one_outcome_in_wire_form() {
        struct Case {
            name: &'static str,
            adds: &'static [(bool, bool, &'static [u8], &'static [u8])],
            drop_receiver: bool,
            finishes: &'static [Finish],
            verbs: &'static [Verb],
            finalized: bool,
            cancelled: bool,
            sent: Sent,
        }
        // Expected values: UnaryResponder.php and Responder.php (one outcome, both halves snapshotted), Work.php (AlreadyFinalizedError, WorkDiscardedException, loss), PROTOCOL-HTTP2 Requests (emit unpadded -bin values) and RFC 4648 (01 02 is `AQI`).
        const CASES: &[Case] = &[
            Case {
                name: "respond carries both halves",
                adds: &[
                    (false, false, b"x-a", b"1"),
                    (false, false, b"x-a", b"2"),
                    (false, true, b"x-b-bin", b"\x01\x02"),
                    (true, false, b"x-t", b"w"),
                ],
                drop_receiver: false,
                finishes: &[Finish::Respond(b"\x0a\x02hi")],
                verbs: &[Verb::Ok],
                finalized: true,
                cancelled: false,
                sent: Sent::Outcome {
                    headers: &[("x-a", "1"), ("x-a", "2"), ("x-b-bin", "AQI")],
                    trailers: &[("x-t", "w")],
                    result: Ok(b"\x0a\x02hi"),
                },
            },
            Case {
                name: "fail carries the status",
                adds: &[],
                drop_receiver: false,
                finishes: &[Finish::Fail((
                    5,
                    "no invoice",
                    &[("type.googleapis.com/google.rpc.ErrorInfo", b"\x0a\x01x")],
                ))],
                verbs: &[Verb::Ok],
                finalized: true,
                cancelled: false,
                sent: Sent::Outcome {
                    headers: &[],
                    trailers: &[],
                    result: Err((
                        5,
                        "no invoice",
                        &[("type.googleapis.com/google.rpc.ErrorInfo", b"\x0a\x01x")],
                    )),
                },
            },
            Case {
                name: "second finalize",
                adds: &[],
                drop_receiver: false,
                finishes: &[Finish::Respond(b"a"), Finish::Respond(b"b")],
                verbs: &[Verb::Ok, Verb::Finalized],
                finalized: true,
                cancelled: false,
                sent: Sent::Outcome {
                    headers: &[],
                    trailers: &[],
                    result: Ok(b"a"),
                },
            },
            Case {
                name: "receiver dropped before respond",
                adds: &[],
                drop_receiver: true,
                finishes: &[Finish::Respond(b"a")],
                verbs: &[Verb::Discarded],
                finalized: true,
                cancelled: true,
                sent: Sent::Nothing,
            },
            Case {
                name: "unfinalized drop",
                adds: &[],
                drop_receiver: false,
                finishes: &[],
                verbs: &[],
                finalized: false,
                cancelled: false,
                sent: Sent::Lost,
            },
        ];
        for c in CASES {
            let (mut st, rx) = state();
            let mut rx = (!c.drop_receiver).then_some(rx);
            for &(trailer, binary, name, value) in c.adds {
                assert_eq!(
                    add_core(Some(&mut st), trailer, binary, name, value),
                    Verb::Ok,
                    "{}",
                    c.name
                );
            }
            let verbs: Vec<Verb> = c
                .finishes
                .iter()
                .map(|f| match f {
                    Finish::Respond(message) => finish(&mut st, Ok(Bytes::from_static(message))),
                    Finish::Fail((code, message, details)) => {
                        finish(&mut st, Err(status(*code, message, details)))
                    }
                })
                .collect();
            assert_eq!(verbs, c.verbs, "{}", c.name);
            assert_eq!(st.is_finalized(), c.finalized, "{}: finalized", c.name);
            assert_eq!(st.is_cancelled(), c.cancelled, "{}: cancelled", c.name);
            drop(st);

            match &c.sent {
                Sent::Outcome {
                    headers,
                    trailers,
                    result,
                } => {
                    let want = GrpcOutcome {
                        headers: map(headers),
                        trailers: map(trailers),
                        result: result
                            .map(Bytes::from_static)
                            .map_err(|(code, message, details)| status(code, message, details)),
                    };
                    let got = rx.as_mut().map(|rx| rx.try_recv());
                    assert_eq!(got, Some(Ok(want)), "{}", c.name);
                }
                Sent::Lost => {
                    let got = rx.as_mut().map(|rx| rx.try_recv());
                    assert_eq!(
                        got,
                        Some(Err(oneshot::error::TryRecvError::Closed)),
                        "{}",
                        c.name
                    );
                }
                Sent::Nothing => assert!(rx.is_none(), "{}", c.name),
            }
        }
    }
}
