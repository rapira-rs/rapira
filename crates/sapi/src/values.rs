use crate::{
    IS_OBJECT, callbacks::guard, rapira_ce_inet_address, rapira_ce_tls, rapira_ce_unix_address,
    zend, zend_object, zend_string, zend_zval_value_name, zval,
};

/// Checks the `Rapira\InetAddress|Rapira\UnixAddress` union that arginfo cannot enforce (internal arg types are debug-only).
/// # Safety
/// `zv` a live zval; engine active on this thread.
pub unsafe fn address_arg(zv: *mut zval, num: u32) -> bool {
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
