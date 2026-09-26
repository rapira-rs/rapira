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
    IS_OBJECT,
    api::{RpcProtocol, RpcStatus, UnaryCall, UnaryReply},
    rapira_ce_already_finalized_error, rapira_ce_grpc_context, rapira_ce_grpc_error_detail,
    rapira_ce_grpc_metadata, rapira_ce_grpc_method_info, rapira_ce_grpc_method_kind,
    rapira_ce_grpc_protocol, rapira_ce_grpc_service_info, rapira_ce_grpc_status,
    rapira_ce_internal_grpc_response_metadata, rapira_ce_work_discarded_exception,
    rapira_grpc_call_obj, rapira_grpc_metadata_obj, rapira_zval_enum_case,
    types::{GrpcJob, GrpcMethod, GrpcService, MethodKind},
    zend_argument_type_error, zend_read_property, zend_zval_value_name,
};

thread_local! {
    /// The services of the gRPC pool this PHP thread serves.
    static SERVICES: RefCell<Option<Vec<GrpcService>>> = const { RefCell::new(None) };
}

pub(super) fn set_services(services: Vec<GrpcService>) {
    SERVICES.set(Some(services));
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
            MethodKind::of(m).case().as_ptr(),
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

/// The Metadata rule for a text value: printable ASCII, 0x20 to 0x7E.
pub(crate) fn printable(value: &[u8]) -> bool {
    value.iter().all(|b| (0x20..=0x7e).contains(b))
}

/// A metadata name that carries binary values.
pub(crate) fn is_binary(name: &[u8]) -> bool {
    name.ends_with(b"-bin")
}

/// `Context::$metadata`: the application keys, with `-bin` values decoded. A text value that is not printable or an undecodable `-bin` value is dropped, and a key with no values left is absent.
/// A `-bin` value is split on "," first: "Implementations must split Binary-Headers on "," before decoding the Base64-encoded values." https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
/// The optional whitespace around "," follows the HTTP list rule. https://www.rfc-editor.org/rfc/rfc9110#section-5.6.1
fn context_metadata(headers: &HeaderMap) -> Fields {
    let mut out = Fields::new();
    for name in headers.keys() {
        if reserved(name.as_str()) {
            continue;
        }
        let binary = is_binary(name.as_str().as_bytes());
        let mut values: Vec<Vec<u8>> = Vec::new();
        for v in headers.get_all(name) {
            let v = v.as_bytes();
            if !binary {
                if printable(v) {
                    values.push(v.to_vec());
                } else {
                    tracing::debug!(target: "rapira", "dropped a non-printable {name} value");
                }
                continue;
            }
            for piece in v.split(|&b| b == b',').map(<[u8]>::trim_ascii) {
                match BIN_DECODE.decode(piece) {
                    Ok(decoded) => values.push(decoded),
                    Err(e) => {
                        tracing::debug!(target: "rapira", "dropped an undecodable {name} value: {e}");
                    }
                }
            }
        }
        if !values.is_empty() {
            out.push((name.clone(), values));
        }
    }
    out
}

/// Response metadata in wire form. `-bin` values are base64 without padding: implementations "should emit un-padded values" (PROTOCOL-HTTP2 Requests).
/// A text value loses its edge spaces: an HTTP/2 field value "MUST NOT start or end with an ASCII whitespace character" (https://www.rfc-editor.org/rfc/rfc9113#section-8.2.1), and for gRPC "it may be stripped" (PROTOCOL-HTTP2 Requests).
fn wire(fields: &[Field]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, values) in fields {
        let binary = is_binary(name.as_str().as_bytes());
        for v in values {
            let value = if binary {
                HeaderValue::try_from(STANDARD_NO_PAD.encode(v))
            } else {
                HeaderValue::from_bytes(v.trim_ascii())
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
    req: UnaryCall,
    received_at: f64,
    /// None after the outcome went out or the call was discarded: the call is then finalized.
    reply: Option<oneshot::Sender<UnaryReply>>,
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
            discarded: false,
            headers: Fields::new(),
            trailers: Fields::new(),
            view: None,
        }
    }
}

impl Held for GrpcState {
    fn finalized(&self) -> bool {
        self.reply.is_none()
    }

    /// The host dropped the receiver: the deadline passed or the client left.
    fn host_closed(&self) -> bool {
        self.discarded || self.reply.as_ref().is_some_and(oneshot::Sender::is_closed)
    }

    /// Dropping `reply` finalizes the call, so `rapira_rs_grpc_drop`/`reclaim_current` do not count it a second time.
    fn discard(&mut self) {
        if self.finalized() {
            return;
        }
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
    // Header-Name: https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
    if !name
        .as_str()
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'_' | b'-' | b'.'))
    {
        return Verb::BadField(c"a metadata name must use only 0-9 a-z _ - .");
    }
    if reserved(name.as_str()) {
        return Verb::BadField(c"the metadata name is reserved for the transport");
    }
    match (binary, is_binary(name.as_str().as_bytes())) {
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
    if !binary && !printable(value) {
        return Verb::BadField(c"a text metadata value must be printable ASCII");
    }
    let Some(st) = st else {
        return Verb::Finalized;
    };
    // a call that the host closed keeps the entry, and finish() gives WorkDiscardedException
    if st.finalized() {
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
fn finish(st: &mut GrpcState, result: Result<Bytes, RpcStatus>) -> Verb {
    if st.host_closed() {
        st.discard();
        return Verb::Discarded;
    }
    if st.finalized() {
        return Verb::Finalized;
    }
    let outcome = UnaryReply {
        headers: wire(&st.headers),
        trailers: wire(&st.trailers),
        outcome: result,
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

/// The messages of the two finalize errors name the call, as the Responder.php `@throws` lines do.
/// # Safety
/// Engine active; can bailout on OOM.
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
            v => throw_verb(v),
        }
    }
    false
}

container_of!(pub(super) grpc_call_from, rapira_grpc_call_obj);
container_of!(metadata_from, rapira_grpc_metadata_obj);

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

fn protocol_case(protocol: RpcProtocol) -> &'static CStr {
    match protocol {
        RpcProtocol::Grpc => c"Grpc",
        RpcProtocol::GrpcWeb => c"GrpcWeb",
        RpcProtocol::Connect => c"Connect",
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
        let zv = read_prop(rapira_ce_grpc_error_detail, obj, name);
        (zend::zval_type(zv) == IS_STRING).then(|| zend::zstr_bytes((*zv).value.str_))
    }
}

/// A declared property of `obj`, read through the engine.
/// # Safety
/// `obj` an instance of `ce`; engine active.
unsafe fn read_prop(ce: *mut zend_class_entry, obj: *mut zend_object, name: &CStr) -> *mut zval {
    unsafe {
        let mut rv: zval = std::mem::zeroed();
        zend::deref(zend_read_property(
            ce,
            obj,
            name.as_ptr(),
            name.count_bytes(),
            true,
            &mut rv,
        ))
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
        (*state.cast::<GrpcState>()).host_closed()
    })
}

/// The state keeps the message; the C shell copies it once per call object.
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

/// `status` is a `Rapira\Grpc\Status`; its constructor typed the three properties. A Status from ReflectionClass::newInstanceWithoutConstructor() is not supported. A detail that is not an ErrorDetail gives a TypeError before any state change.
/// # Safety
/// `state` from receive; `status` a live object; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_fail(state: *mut c_void, status: *mut zend_object) -> bool {
    guard(false, || unsafe {
        let code = read_prop(rapira_ce_grpc_status, status, c"code");
        let code = read_prop((*(*code).value.obj).ce, (*code).value.obj, c"value");
        let message = read_prop(rapira_ce_grpc_status, status, c"message");
        let details = read_prop(rapira_ce_grpc_status, status, c"details");
        let details = match read_details((*details).value.arr) {
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
        let status = RpcStatus {
            code: (*code).value.lval as u32,
            message: String::from_utf8_lossy(zend::zstr_bytes((*message).value.str_)).into_owned(),
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
    guard((), || drop(unsafe { release(state.cast::<GrpcState>()) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (GrpcState, oneshot::Receiver<UnaryReply>) {
        let (reply, rx) = oneshot::channel();
        let req = UnaryCall {
            method: "rapira.test.v1.EchoService/Echo".into(),
            protocol: RpcProtocol::Grpc,
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

    /// `from_bytes` takes the obs-text bytes that a client can send.
    fn map(lines: &[(&'static str, &'static str)]) -> HeaderMap {
        lines
            .iter()
            .map(|&(k, v)| {
                let value = HeaderValue::from_bytes(v.as_bytes()).expect("a valid field value");
                (HeaderName::from_static(k), value)
            })
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
        for (header, reserved) in [
            ("grpc-timeout", true),
            ("grpc-status-details-bin", true),
            ("content-type", true),
            ("connect-timeout-ms", true),
            ("trailer-x", true),
            ("te", true),
            ("trailer", true),
            ("host", true),
            ("connection", true),
            ("keep-alive", true),
            ("proxy-connection", true),
            ("transfer-encoding", true),
            ("upgrade", true),
            ("accept-encoding", true),
            ("user-agent", false),
            ("x-user", false),
            ("authorization", false),
            ("grpcx", false),
            ("x-grpc-web", false),
        ] {
            assert_eq!(super::reserved(header), reserved, "{header}");
        }
    }

    #[test]
    fn context_metadata_filters_and_decodes() {
        struct Case {
            name: &'static str,
            headers: &'static [(&'static str, &'static str)],
            expected: &'static [(&'static str, &'static [&'static [u8]])],
        }
        // Sources: PROTOCOL-HTTP2 Requests (padded and unpadded -bin values, split on ",", duplicates joined with ","), grpc-go decodeMetadataHeader (an empty -bin value is an empty value), RFC 9110 5.6.1 (whitespace around the ","), RFC 4648 (`AP8` is 00 ff, `AQI` is 01 02).
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
                name: "comma-separated -bin values split",
                headers: &[("x-trace-bin", "AP8,AQI")],
                expected: &[("x-trace-bin", &[b"\x00\xff", b"\x01\x02"])],
            },
            Case {
                name: "comma-separated -bin values with a space split",
                headers: &[("x-trace-bin", "AP8, AQI")],
                expected: &[("x-trace-bin", &[b"\x00\xff", b"\x01\x02"])],
            },
            Case {
                name: "empty -bin value kept",
                headers: &[("x-token-bin", "")],
                expected: &[("x-token-bin", &[b""])],
            },
            Case {
                name: "empty -bin value between commas kept",
                headers: &[("x-trace-bin", "AP8,,AQI")],
                expected: &[("x-trace-bin", &[b"\x00\xff", b"", b"\x01\x02"])],
            },
            Case {
                name: "non-ASCII text value dropped",
                headers: &[("x-user", "a"), ("x-user", "\u{e9}")],
                expected: &[("x-user", &[b"a"])],
            },
            Case {
                name: "text value with a tab dropped",
                headers: &[("x-user", "a\tb"), ("x-user", "c")],
                expected: &[("x-user", &[b"c"])],
            },
            Case {
                name: "key with only a non-printable value absent",
                headers: &[("x-user", "\u{e9}")],
                expected: &[],
            },
            Case {
                name: "reserved names dropped",
                headers: &[("grpc-timeout", "1S"), ("te", "trailers")],
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

    #[test]
    fn wire_trims_text_values() {
        struct Case {
            name: &'static str,
            field: (&'static str, &'static [u8]),
            expected: &'static str,
        }
        // Sources: PROTOCOL-HTTP2 Requests ("ASCII-Value should not have leading or trailing whitespace. If it contains leading or trailing whitespace, it may be stripped."), RFC 9113 8.2.1 (a field value must not start or end with whitespace), RFC 4648 (`IA` is one space).
        const CASES: &[Case] = &[
            Case {
                name: "leading space stripped",
                field: ("x-cost", b"  12.30"),
                expected: "12.30",
            },
            Case {
                name: "trailing space stripped",
                field: ("x-note", b"done "),
                expected: "done",
            },
            Case {
                name: "inner space kept",
                field: ("x-note", b"a b"),
                expected: "a b",
            },
            Case {
                name: "only spaces become empty",
                field: ("x-note", b"   "),
                expected: "",
            },
            Case {
                name: "-bin value is encoded, not trimmed",
                field: ("x-b-bin", b" "),
                expected: "IA",
            },
        ];
        for c in CASES {
            let (name, value) = c.field;
            let fields = vec![(HeaderName::from_static(name), vec![value.to_vec()])];
            let map = wire(&fields);
            assert_eq!(
                map.get(name).map(HeaderValue::as_bytes),
                Some(c.expected.as_bytes()),
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
        BadField(&'static CStr),
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
        // Sources: ResponseMetadata.php, PROTOCOL-HTTP2 Header-Name (0-9 a-z _ - .). A bad field is a ValueError before any state check.
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
                name: "reserved name refused",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"grpc-status",
                value: b"0",
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
                name: "plus in the name",
                setup: Setup::Open,
                trailer: false,
                binary: false,
                key: b"x+debug",
                value: b"a",
                want: Want::BadField(c"a metadata name must use only 0-9 a-z _ - ."),
            },
            Case {
                name: "exclamation mark in the name",
                setup: Setup::Open,
                trailer: true,
                binary: false,
                key: b"x!a",
                value: b"a",
                want: Want::BadField(c"a metadata name must use only 0-9 a-z _ - ."),
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
                want: Want::Stored("x-a", b"a"),
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
                Want::BadField(msg) => assert_eq!(v, Verb::BadField(msg), "{}", c.name),
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

    fn status(code: u32, message: &str, details: &[(&str, &[u8])]) -> RpcStatus {
        RpcStatus {
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
        // Sources: Responder.php and Work.php, PROTOCOL-HTTP2 Requests (unpadded -bin values), RFC 4648 (01 02 is `AQI`).
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
            assert_eq!(st.host_closed(), c.cancelled, "{}: cancelled", c.name);
            drop(st);

            match &c.sent {
                Sent::Outcome {
                    headers,
                    trailers,
                    result,
                } => {
                    let want = UnaryReply {
                        headers: map(headers),
                        trailers: map(trailers),
                        outcome: result
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
