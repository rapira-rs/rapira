use std::ffi::{CStr, c_char};

use crate::{
    IS_NULL, IS_REFERENCE, IS_UNDEF, rapira_eg, zend_class_entry, zend_object, zend_string,
    zend_throw_error, zend_throw_exception, zend_update_property, zend_update_property_double,
    zend_update_property_long, zend_update_property_null, zend_update_property_stringl,
    zend_value_error, zval,
};

pub(crate) fn ptr_or_empty(bytes: &[u8]) -> *const c_char {
    if bytes.is_empty() {
        c"".as_ptr()
    } else {
        bytes.as_ptr().cast()
    }
}

/// # Safety
/// `zv` readable.
pub(crate) unsafe fn is_undef(zv: *const zval) -> bool {
    unsafe { u32::from((*zv).u1.v.type_) == IS_UNDEF }
}

/// # Safety
/// `list` a live packed array; ownership of the string bytes stays with the caller.
pub(crate) unsafe fn list_push_stringl(list: *mut zval, bytes: &[u8]) {
    unsafe {
        crate::add_next_index_stringl(list, ptr_or_empty(bytes), bytes.len());
    }
}

/// # Safety
/// `ce` a registered class; `obj` alive; the property declared on `ce`.
pub(crate) unsafe fn prop_stringl(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    bytes: &[u8],
) {
    unsafe {
        zend_update_property_stringl(
            ce,
            obj,
            name.as_ptr(),
            name.count_bytes(),
            ptr_or_empty(bytes),
            bytes.len(),
        );
    }
}

/// # Safety
/// As `prop_stringl`.
pub(crate) unsafe fn prop_str_or_null(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    bytes: Option<&[u8]>,
) {
    unsafe {
        match bytes {
            Some(b) => prop_stringl(ce, obj, name, b),
            None => prop_null(ce, obj, name),
        }
    }
}

/// # Safety
/// As `prop_stringl`.
pub(crate) unsafe fn prop_null(ce: *mut zend_class_entry, obj: *mut zend_object, name: &CStr) {
    unsafe { zend_update_property_null(ce, obj, name.as_ptr(), name.count_bytes()) }
}

/// # Safety
/// As `prop_stringl`.
pub(crate) unsafe fn prop_long(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    v: i64,
) {
    unsafe { zend_update_property_long(ce, obj, name.as_ptr(), name.count_bytes(), v) }
}

/// # Safety
/// As `prop_stringl`.
pub(crate) unsafe fn prop_double(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    v: f64,
) {
    unsafe { zend_update_property_double(ce, obj, name.as_ptr(), name.count_bytes(), v) }
}

/// `zend_update_property` addrefs `zv`; the caller keeps its ref and dtors it.
/// # Safety
/// As `prop_stringl`; `zv` initialized.
pub(crate) unsafe fn prop_zval(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    zv: *mut zval,
) {
    unsafe { zend_update_property(ce, obj, name.as_ptr(), name.count_bytes(), zv) }
}

/// `zend_update_property_str` shares `val` (addref), no byte copy.
/// # Safety
/// As `prop_stringl`; `val` a live zend_string.
pub(crate) unsafe fn prop_zstr(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    val: *mut zend_string,
) {
    unsafe { crate::zend_update_property_str(ce, obj, name.as_ptr(), name.count_bytes(), val) }
}

/// # Safety
/// As `prop_zstr`; `val` NULL registers the property null.
pub(crate) unsafe fn prop_zstr_or_null(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    val: *mut zend_string,
) {
    unsafe {
        if val.is_null() {
            prop_null(ce, obj, name);
        } else {
            prop_zstr(ce, obj, name, val);
        }
    }
}

/// # Safety
/// Engine booted on this thread.
pub(crate) unsafe fn exception_pending() -> bool {
    unsafe { !(*rapira_eg()).exception.is_null() }
}

/// instanceof_function is inline; this is its two-halves replication.
/// # Safety
/// Both class entries registered.
pub(crate) unsafe fn instanceof(
    ce: *const zend_class_entry,
    base: *const zend_class_entry,
) -> bool {
    ce == base || unsafe { crate::instanceof_function_slow(ce, base) }
}

/// # Safety
/// `s` a live zend_string; the borrow must not outlive it.
pub(crate) unsafe fn zstr_bytes<'a>(s: *const zend_string) -> &'a [u8] {
    unsafe { std::slice::from_raw_parts((*s).val.as_ptr().cast::<u8>(), (*s).len) }
}

/// # Safety
/// `zv` readable.
pub(crate) unsafe fn zval_type(zv: *const zval) -> u32 {
    unsafe { u32::from((*zv).u1.v.type_) }
}

/// # Safety
/// `zv` writable.
pub(crate) unsafe fn zval_null(zv: *mut zval) {
    unsafe {
        (*zv).u1.type_info = IS_NULL;
    }
}

/// # Safety
/// `zv` a live zval; a reference's payload stays owned by the reference.
pub(crate) unsafe fn deref(zv: *mut zval) -> *mut zval {
    unsafe {
        if zval_type(zv) == IS_REFERENCE {
            &raw mut (*(*zv).value.ref_).val
        } else {
            zv
        }
    }
}

/// # Safety
/// Engine active on this thread; can bailout on OOM like any allocating call.
pub(crate) unsafe fn throw_error(msg: &CStr) {
    unsafe {
        zend_throw_error(std::ptr::null_mut(), c"%s".as_ptr(), msg.as_ptr());
    }
}

/// # Safety
/// As `throw_error`.
pub(crate) unsafe fn throw_value_error(msg: &CStr) {
    unsafe {
        zend_value_error(c"%s".as_ptr(), msg.as_ptr());
    }
}

/// # Safety
/// As `throw_error`; `ce` a registered exception class.
pub(crate) unsafe fn throw_exception(ce: *mut zend_class_entry, msg: &CStr) {
    unsafe {
        zend_throw_exception(ce, msg.as_ptr(), 0);
    }
}
