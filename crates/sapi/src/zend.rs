use std::ffi::{CStr, c_char};

use crate::{
    IS_DOUBLE, IS_LONG, IS_NULL, IS_PROP_REINITABLE, IS_PROP_UNINIT, IS_REFERENCE, IS_UNDEF,
    rapira_cg, rapira_eg, rapira_zval_stringl, zend_class_entry, zend_object, zend_property_info,
    zend_string, zend_throw_error, zend_throw_exception, zend_update_property,
    zend_update_property_double, zend_update_property_long, zend_update_property_null,
    zend_update_property_stringl, zend_value_error, zval,
};

pub fn ptr_or_empty(bytes: &[u8]) -> *const c_char {
    if bytes.is_empty() {
        c"".as_ptr()
    } else {
        bytes.as_ptr().cast()
    }
}

/// # Safety
/// `zv` readable.
pub unsafe fn is_undef(zv: *const zval) -> bool {
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
pub unsafe fn prop_stringl(
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
pub unsafe fn prop_str_or_null(
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
pub unsafe fn prop_null(ce: *mut zend_class_entry, obj: *mut zend_object, name: &CStr) {
    unsafe { zend_update_property_null(ce, obj, name.as_ptr(), name.count_bytes()) }
}

/// # Safety
/// As `prop_stringl`.
pub unsafe fn prop_long(ce: *mut zend_class_entry, obj: *mut zend_object, name: &CStr, v: i64) {
    unsafe { zend_update_property_long(ce, obj, name.as_ptr(), name.count_bytes(), v) }
}

/// # Safety
/// As `prop_stringl`.
pub unsafe fn prop_double(ce: *mut zend_class_entry, obj: *mut zend_object, name: &CStr, v: f64) {
    unsafe { zend_update_property_double(ce, obj, name.as_ptr(), name.count_bytes(), v) }
}

/// `zend_update_property` addrefs `zv`; the caller keeps its ref and dtors it.
/// # Safety
/// As `prop_stringl`; `zv` initialized.
pub unsafe fn prop_zval(
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
pub unsafe fn prop_zstr(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    val: *mut zend_string,
) {
    unsafe { crate::zend_update_property_str(ce, obj, name.as_ptr(), name.count_bytes(), val) }
}

/// # Safety
/// As `prop_zstr`; `val` NULL registers the property null.
pub unsafe fn prop_zstr_or_null(
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

/// The slot offset of the property `name` declared on `ce`. Panics when `ce` declares no such property.
/// # Safety
/// `ce` a registered class.
pub unsafe fn prop_offset(ce: *mut zend_class_entry, name: &CStr) -> u32 {
    unsafe {
        let zv = crate::zend_hash_str_find(
            &raw const (*ce).properties_info,
            name.as_ptr(),
            name.count_bytes(),
        );
        assert!(!zv.is_null(), "{name:?} is not a declared property");
        (*(*zv).value.ptr.cast::<zend_property_info>()).offset
    }
}

/// Moves `value` into the property slot at `offset` (OBJ_PROP in Zend/zend_object_handlers.h) and clears IS_PROP_UNINIT and IS_PROP_REINITABLE, as the first write in zend_std_write_property does. The slot takes the caller's ref.
/// # Safety
/// `obj` a new object whose slot at `offset` is uninitialized; `value` holds the declared type of the property.
pub unsafe fn slot_init(obj: *mut zend_object, offset: u32, value: *const zval) {
    unsafe {
        let slot = obj.byte_add(offset as usize).cast::<zval>();
        (*slot).value = (*value).value;
        (*slot).u1 = (*value).u1;
        (*slot).u2.extra &= !(IS_PROP_UNINIT | IS_PROP_REINITABLE);
    }
}

/// # Safety
/// As `slot_init`; the property type accepts a string.
pub unsafe fn slot_stringl(obj: *mut zend_object, offset: u32, bytes: &[u8]) {
    unsafe {
        let mut v: zval = std::mem::zeroed();
        rapira_zval_stringl(&mut v, ptr_or_empty(bytes), bytes.len());
        slot_init(obj, offset, &v);
    }
}

/// # Safety
/// As `slot_init`; the property type accepts a string and null.
pub unsafe fn slot_str_or_null(obj: *mut zend_object, offset: u32, bytes: Option<&[u8]>) {
    unsafe {
        match bytes {
            Some(b) => slot_stringl(obj, offset, b),
            None => slot_null(obj, offset),
        }
    }
}

/// # Safety
/// As `slot_init`; the property type accepts null.
pub unsafe fn slot_null(obj: *mut zend_object, offset: u32) {
    unsafe {
        let mut v: zval = std::mem::zeroed();
        v.u1.type_info = IS_NULL;
        slot_init(obj, offset, &v);
    }
}

/// # Safety
/// As `slot_init`; the property type is int.
pub unsafe fn slot_long(obj: *mut zend_object, offset: u32, n: i64) {
    unsafe {
        let mut v: zval = std::mem::zeroed();
        v.value.lval = n;
        v.u1.type_info = IS_LONG;
        slot_init(obj, offset, &v);
    }
}

/// # Safety
/// As `slot_init`; the property type is float.
pub unsafe fn slot_double(obj: *mut zend_object, offset: u32, d: f64) {
    unsafe {
        let mut v: zval = std::mem::zeroed();
        v.value.dval = d;
        v.u1.type_info = IS_DOUBLE;
        slot_init(obj, offset, &v);
    }
}

/// # Safety
/// Engine booted on this thread.
pub unsafe fn exception_pending() -> bool {
    unsafe { !(*rapira_eg()).exception.is_null() }
}

/// instanceof_function is inline; this is its two-halves replication.
/// # Safety
/// Both class entries registered.
pub unsafe fn instanceof(ce: *const zend_class_entry, base: *const zend_class_entry) -> bool {
    ce == base || unsafe { crate::instanceof_function_slow(ce, base) }
}

/// # Safety
/// `s` a live zend_string; the borrow must not outlive it.
pub unsafe fn zstr_bytes<'a>(s: *const zend_string) -> &'a [u8] {
    unsafe { std::slice::from_raw_parts((*s).val.as_ptr().cast::<u8>(), (*s).len) }
}

/// # Safety
/// `zv` readable.
pub unsafe fn zval_type(zv: *const zval) -> u32 {
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
pub unsafe fn deref(zv: *mut zval) -> *mut zval {
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
pub unsafe fn throw_error(msg: &CStr) {
    unsafe {
        zend_throw_error(std::ptr::null_mut(), c"%s".as_ptr(), msg.as_ptr());
    }
}

/// # Safety
/// As `throw_error`.
pub unsafe fn throw_value_error(msg: &CStr) {
    unsafe {
        zend_value_error(c"%s".as_ptr(), msg.as_ptr());
    }
}

/// # Safety
/// As `throw_error`; `ce` a registered exception class.
pub unsafe fn throw_exception(ce: *mut zend_class_entry, msg: &CStr) {
    unsafe {
        zend_throw_exception(ce, msg.as_ptr(), 0);
    }
}

/// The class entry of `name`, given without the leading backslash, or null.
/// # Safety
/// As `class_exists`.
unsafe fn find_class(name: &str) -> *mut zend_class_entry {
    let key = name.to_ascii_lowercase();
    // CG(class_table): EG(class_table) is set only when a request starts (init_executor), so the master has none.
    unsafe {
        let zv =
            crate::zend_hash_str_find((*rapira_cg()).class_table, key.as_ptr().cast(), key.len());
        if zv.is_null() {
            std::ptr::null_mut()
        } else {
            (*zv).value.ptr.cast()
        }
    }
}

/// Whether the class table holds `name`, given without the leading backslash.
/// # Safety
/// Call it between `boot_master` and the drop of the module it returned: CG(class_table) is live only in that window.
pub unsafe fn class_exists(name: &str) -> bool {
    unsafe { !find_class(name).is_null() }
}

/// Whether `child` is `parent`, extends it or implements it. False when either class is missing.
/// # Safety
/// As `class_exists`.
pub unsafe fn class_extends(child: &str, parent: &str) -> bool {
    unsafe {
        let (child, parent) = (find_class(child), find_class(parent));
        !child.is_null() && !parent.is_null() && instanceof(child, parent)
    }
}
