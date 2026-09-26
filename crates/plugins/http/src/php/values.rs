use std::ffi::c_double;

use rapira_sapi::callbacks::guard;
use rapira_sapi::values::address_arg;
use rapira_sapi::{zend, zend_object, zend_string, zval};

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
