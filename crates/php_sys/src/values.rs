use std::ffi::{c_char, c_double};
use std::ptr::null_mut;

use crate::{
    HASH_KEY_IS_STRING, HashPosition, HashTable, IS_ARRAY, IS_OBJECT, IS_STRING, callbacks::guard,
    object_init_ex, rapira_array_init, rapira_ce_grpc_exception, rapira_ce_grpc_status,
    rapira_ce_inet_address, rapira_ce_tls, rapira_ce_unix_address, rapira_symtable_str_find, zend,
    zend_argument_type_error, zend_argument_value_error, zend_get_exception_base,
    zend_hash_get_current_data_ex, zend_hash_get_current_key_ex,
    zend_hash_internal_pointer_reset_ex, zend_hash_move_forward_ex, zend_object, zend_string,
    zend_zval_value_name, zval, zval_add_ref, zval_ptr_dtor,
};

/// Checks the `Rapira\InetAddress|Rapira\UnixAddress` union that arginfo cannot enforce (internal arg types are debug-only).
unsafe fn address_arg(zv: *mut zval, num: u32) -> bool {
    unsafe {
        if zend::zval_type(zv) == IS_OBJECT {
            let ce = (*(*zv).value.obj).ce;
            if zend::instanceof(ce, rapira_ce_inet_address)
                || zend::instanceof(ce, rapira_ce_unix_address)
            {
                return true;
            }
        }
        crate::zend_argument_type_error(
            num,
            c"must be of type Rapira\\InetAddress|Rapira\\UnixAddress, %s given".as_ptr(),
            zend_zval_value_name(zv),
        );
        false
    }
}

/// # Safety
/// `obj` is under construction; strings/zvals are ZPP-owned for the call, and all ctors below share this contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_inet_address(
    obj: *mut zend_object,
    ip: *mut zend_string,
    port: i64,
) -> bool {
    guard(false, || unsafe {
        let ce = rapira_ce_inet_address;
        zend::prop_zstr(ce, obj, c"ip", ip);
        zend::prop_long(ce, obj, c"port", port);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_inet_address`; `path` nullable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_unix_address(
    obj: *mut zend_object,
    path: *mut zend_string,
) -> bool {
    guard(false, || unsafe {
        zend::prop_zstr_or_null(rapira_ce_unix_address, obj, c"path", path);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_inet_address`; the five cert/negotiation strings are nullable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_tls(
    obj: *mut zend_object,
    version: *mut zend_string,
    cipher: *mut zend_string,
    negotiated: *mut zend_string,
    server_name: *mut zend_string,
    serial: *mut zend_string,
    org: *mut zend_string,
    fingerprint: *mut zend_string,
) -> bool {
    guard(false, || unsafe {
        let ce = rapira_ce_tls;
        zend::prop_zstr(ce, obj, c"version", version);
        zend::prop_zstr(ce, obj, c"cipher", cipher);
        zend::prop_zstr_or_null(ce, obj, c"negotiatedProtocol", negotiated);
        zend::prop_zstr_or_null(ce, obj, c"requestedServerName", server_name);
        zend::prop_zstr_or_null(ce, obj, c"certSerial", serial);
        zend::prop_zstr_or_null(ce, obj, c"certOrganization", org);
        zend::prop_zstr_or_null(ce, obj, c"certFingerprint", fingerprint);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_inet_address`; `headers` a live array zval.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_form_field(
    obj: *mut zend_object,
    name: *mut zend_string,
    value: *mut zend_string,
    headers: *mut zval,
) -> bool {
    guard(false, || unsafe {
        let ce = (*obj).ce;
        zend::prop_zstr(ce, obj, c"name", name);
        zend::prop_zstr(ce, obj, c"value", value);
        zend::prop_zval(ce, obj, c"headers", headers);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_form_field`; `client_media_type` nullable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_uploaded_file(
    obj: *mut zend_object,
    name: *mut zend_string,
    client_filename: *mut zend_string,
    client_media_type: *mut zend_string,
    headers: *mut zval,
    tmp_path: *mut zend_string,
    size: i64,
) -> bool {
    guard(false, || unsafe {
        let ce = (*obj).ce;
        zend::prop_zstr(ce, obj, c"name", name);
        zend::prop_zstr(ce, obj, c"clientFilename", client_filename);
        zend::prop_zstr_or_null(ce, obj, c"clientMediaType", client_media_type);
        zend::prop_zval(ce, obj, c"headers", headers);
        zend::prop_zstr(ce, obj, c"tmpPath", tmp_path);
        zend::prop_long(ce, obj, c"size", size);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_form_field`; `fields`/`files` live array zvals.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_multipart(
    obj: *mut zend_object,
    fields: *mut zval,
    files: *mut zval,
) -> bool {
    guard(false, || unsafe {
        let ce = (*obj).ce;
        zend::prop_zval(ce, obj, c"fields", fields);
        zend::prop_zval(ce, obj, c"files", files);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_form_field`; `body` is a union zval (Multipart object or string), `authority`/`tls` nullable, `remote`/`server` validated here against the address union.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn rapira_rs_ctor_request(
    obj: *mut zend_object,
    method: *mut zend_string,
    uri: *mut zend_string,
    target: *mut zend_string,
    authority: *mut zend_string,
    protocol: *mut zend_string,
    headers: *mut zval,
    body: *mut zval,
    remote: *mut zval,
    server: *mut zval,
    tls: *mut zval,
    received_at: c_double,
) -> bool {
    guard(false, || unsafe {
        if !address_arg(remote, 8) || !address_arg(server, 9) {
            return false;
        }
        let ce = (*obj).ce;
        zend::prop_zstr(ce, obj, c"method", method);
        zend::prop_zstr(ce, obj, c"uri", uri);
        zend::prop_zstr(ce, obj, c"target", target);
        zend::prop_zstr_or_null(ce, obj, c"authority", authority);
        zend::prop_zstr(ce, obj, c"protocol", protocol);
        zend::prop_zval(ce, obj, c"headers", headers);
        zend::prop_zval(ce, obj, c"body", body);
        zend::prop_zval(ce, obj, c"remote", remote);
        zend::prop_zval(ce, obj, c"server", server);
        if tls.is_null() {
            zend::prop_null(ce, obj, c"tls");
        } else {
            zend::prop_zval(ce, obj, c"tls", tls);
        }
        zend::prop_double(ce, obj, c"receivedAt", received_at);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_inet_address`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_grpc_error_detail(
    obj: *mut zend_object,
    type_url: *mut zend_string,
    value: *mut zend_string,
) -> bool {
    guard(false, || unsafe {
        let ce = (*obj).ce;
        zend::prop_zstr(ce, obj, c"typeUrl", type_url);
        zend::prop_zstr(ce, obj, c"value", value);
        !zend::exception_pending()
    })
}

/// # Safety
/// `obj` a Status under construction; `code` a StatusCode case; `details` a live array zval.
unsafe fn status_props(
    obj: *mut zend_object,
    code: *mut zval,
    message: *mut zend_string,
    details: *mut zval,
) {
    unsafe {
        let ce = (*obj).ce;
        zend::prop_zval(ce, obj, c"code", code);
        zend::prop_zstr(ce, obj, c"message", message);
        zend::prop_zval(ce, obj, c"details", details);
    }
}

/// # Safety
/// As `rapira_rs_ctor_form_field`; `code` a StatusCode case, `details` a live array zval.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_grpc_status(
    obj: *mut zend_object,
    code: *mut zval,
    message: *mut zend_string,
    details: *mut zval,
) -> bool {
    guard(false, || unsafe {
        status_props(obj, code, message, details);
        !zend::exception_pending()
    })
}

/// Throws and returns false on the first value that breaks the Metadata rules: the keys are 0, 1, 2, ... in order, each value is a string, and a text value is printable ASCII (0x20-0x7E).
/// `&raw mut pos`: the pos parameter is *mut on PHP 8.4 and *const on 8.5.
/// # Safety
/// `list` a live array; entries stay ZPP-owned.
unsafe fn metadata_values_valid(list: *mut HashTable, binary: bool) -> bool {
    unsafe {
        let mut pos: HashPosition = 0;
        let mut index = 0;
        zend_hash_internal_pointer_reset_ex(list, &mut pos);
        loop {
            let item = zend_hash_get_current_data_ex(list, &raw mut pos);
            if item.is_null() {
                return true;
            }
            let mut str_key: *mut zend_string = null_mut();
            let mut num_key = 0;
            let kt = zend_hash_get_current_key_ex(list, &mut str_key, &mut num_key, &pos);
            if i64::from(kt) == HASH_KEY_IS_STRING || num_key != index {
                zend_argument_type_error(1, c"must map each key to a list of strings".as_ptr());
                return false;
            }
            index += 1;
            let item = zend::deref(item);
            if zend::zval_type(item) != IS_STRING {
                zend_argument_type_error(
                    1,
                    c"must hold only string values, %s given".as_ptr(),
                    zend_zval_value_name(item),
                );
                return false;
            }
            let bytes = zend::zstr_bytes((*item).value.str_);
            if !binary && !bytes.iter().all(|b| (0x20..=0x7e).contains(b)) {
                zend_argument_value_error(
                    1,
                    c"must hold printable ASCII values under a key without the -bin suffix"
                        .as_ptr(),
                );
                return false;
            }
            zend_hash_move_forward_ex(list, &mut pos);
        }
    }
}

/// Throws and returns false on the first entry that breaks the Metadata rules: a key is non-empty lower-case ASCII, and it maps to a list of strings.
/// A canonical decimal key such as "123" is an int key in a PHP array; its decimal text passes the key rule and has no `-bin` suffix.
/// # Safety
/// `ht` a live array; entries stay ZPP-owned.
unsafe fn metadata_valid(ht: *mut HashTable) -> bool {
    unsafe {
        let mut pos: HashPosition = 0;
        zend_hash_internal_pointer_reset_ex(ht, &mut pos);
        loop {
            let entry = zend_hash_get_current_data_ex(ht, &raw mut pos);
            if entry.is_null() {
                return true;
            }
            let mut str_key: *mut zend_string = null_mut();
            let mut num_key = 0;
            let kt = zend_hash_get_current_key_ex(ht, &mut str_key, &mut num_key, &pos);
            let binary = if i64::from(kt) == HASH_KEY_IS_STRING {
                let key = zend::zstr_bytes(str_key);
                if key.is_empty() || !key.iter().all(|b| b.is_ascii() && !b.is_ascii_uppercase()) {
                    zend_argument_value_error(
                        1,
                        c"must have non-empty lower-case ASCII keys".as_ptr(),
                    );
                    return false;
                }
                key.ends_with(b"-bin")
            } else {
                false
            };
            let list = zend::deref(entry);
            if zend::zval_type(list) != IS_ARRAY {
                zend_argument_type_error(
                    1,
                    c"must map each key to a list of strings, %s given".as_ptr(),
                    zend_zval_value_name(list),
                );
                return false;
            }
            if !metadata_values_valid((*list).value.arr, binary) {
                return false;
            }
            zend_hash_move_forward_ex(ht, &mut pos);
        }
    }
}

/// # Safety
/// As `rapira_rs_ctor_form_field`; `entries` a live array zval.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_grpc_metadata(
    obj: *mut zend_object,
    entries: *mut zval,
) -> bool {
    guard(false, || unsafe {
        if !metadata_valid((*entries).value.arr) {
            return false;
        }
        zend::prop_zval((*obj).ce, obj, c"entries", entries);
        !zend::exception_pending()
    })
}

/// Copies the values of the lower-cased `name` into `rv`, or an empty array when the key is absent.
/// The symtable lookup finds an int key such as 123 by its decimal text.
/// # Safety
/// `entries` the live `$entries` array; `name` points to `len` readable bytes; `rv` the return slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_metadata_values(
    entries: *mut HashTable,
    name: *const c_char,
    len: usize,
    rv: *mut zval,
) -> bool {
    guard(false, || unsafe {
        // the key buffer is freed before the calls that can bail out
        let found = {
            let key = std::slice::from_raw_parts(name.cast::<u8>(), len).to_ascii_lowercase();
            rapira_symtable_str_find(entries, zend::ptr_or_empty(&key), key.len())
        };
        if found.is_null() {
            rapira_array_init(rv, 0);
        } else {
            *rv = *zend::deref(found);
            zval_add_ref(rv);
        }
        true
    })
}

/// Client and bidi streaming stream the request; server and bidi streaming stream the response.
/// # Safety
/// `value` points to `len` readable bytes: the backing value of a MethodKind case.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_kind_streams(
    value: *const c_char,
    len: usize,
    request: bool,
) -> bool {
    guard(false, || {
        let kind = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), len) };
        if request {
            matches!(kind, b"client-streaming" | b"bidi-streaming")
        } else {
            matches!(kind, b"server-streaming" | b"bidi-streaming")
        }
    })
}

/// # Safety
/// As `rapira_rs_ctor_inet_address`; `kind` a MethodKind case.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_grpc_method_info(
    obj: *mut zend_object,
    name: *mut zend_string,
    input_type: *mut zend_string,
    output_type: *mut zend_string,
    kind: *mut zval,
) -> bool {
    guard(false, || unsafe {
        let ce = (*obj).ce;
        zend::prop_zstr(ce, obj, c"name", name);
        zend::prop_zstr(ce, obj, c"inputType", input_type);
        zend::prop_zstr(ce, obj, c"outputType", output_type);
        zend::prop_zval(ce, obj, c"kind", kind);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_form_field`; `methods` a live array zval.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_grpc_service_info(
    obj: *mut zend_object,
    name: *mut zend_string,
    methods: *mut zval,
) -> bool {
    guard(false, || unsafe {
        let ce = (*obj).ce;
        zend::prop_zstr(ce, obj, c"name", name);
        zend::prop_zval(ce, obj, c"methods", methods);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_form_field`; `deadline` and `tls` nullable, `remote` validated here against the address union.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn rapira_rs_ctor_grpc_context(
    obj: *mut zend_object,
    method: *mut zend_string,
    metadata: *mut zval,
    deadline: *const c_double,
    remote: *mut zval,
    tls: *mut zval,
    protocol: *mut zval,
    received_at: c_double,
) -> bool {
    guard(false, || unsafe {
        if !address_arg(remote, 4) {
            return false;
        }
        let ce = (*obj).ce;
        zend::prop_zstr(ce, obj, c"method", method);
        zend::prop_zval(ce, obj, c"metadata", metadata);
        match deadline.as_ref() {
            Some(d) => zend::prop_double(ce, obj, c"deadline", *d),
            None => zend::prop_null(ce, obj, c"deadline"),
        }
        zend::prop_zval(ce, obj, c"remote", remote);
        if tls.is_null() {
            zend::prop_null(ce, obj, c"tls");
        } else {
            zend::prop_zval(ce, obj, c"tls", tls);
        }
        zend::prop_zval(ce, obj, c"protocol", protocol);
        zend::prop_double(ce, obj, c"receivedAt", received_at);
        !zend::exception_pending()
    })
}

/// Writes `message` in the exception base scope, as `Exception::__construct` does. Writes the readonly `status` in the GrpcException scope, so a userland subclass can call `parent::__construct()`.
/// # Safety
/// As `rapira_rs_ctor_grpc_status`; `obj` a GrpcException or a subclass under construction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_grpc_exception(
    obj: *mut zend_object,
    code: *mut zval,
    message: *mut zend_string,
    details: *mut zval,
) -> bool {
    guard(false, || unsafe {
        zend::prop_zstr(zend_get_exception_base(obj), obj, c"message", message);
        let mut status: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut status, rapira_ce_grpc_status);
        status_props(status.value.obj, code, message, details);
        zend::prop_zval(rapira_ce_grpc_exception, obj, c"status", &mut status);
        zval_ptr_dtor(&mut status);
        !zend::exception_pending()
    })
}
