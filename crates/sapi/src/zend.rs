use std::ffi::{CStr, c_char};

use crate::{
    HashPosition, HashTable, IS_DOUBLE, IS_LONG, IS_NULL, IS_PROP_REINITABLE, IS_PROP_UNINIT,
    IS_REFERENCE, IS_UNDEF, executor_globals, rapira_zval_stringl, zend_class_entry,
    zend_hash_get_current_data_ex, zend_hash_get_current_key_ex,
    zend_hash_internal_pointer_reset_ex, zend_hash_move_forward_ex, zend_object,
    zend_property_info, zend_read_property, zend_string, zend_throw_error, zend_throw_exception,
    zend_ulong, zend_update_property, zend_update_property_double, zend_update_property_long,
    zend_update_property_null, zend_update_property_stringl, zend_value_error, zval,
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
    unsafe { !executor_globals.exception.is_null() }
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

/// The engine writes a `__get` or hook result into `rv` and returns a pointer into it, so `rv` must outlive the returned pointer. Never NULL: an unset property gives `EG(uninitialized_zval)`.
/// # Safety
/// `ce` a registered class; `obj` alive; engine active.
pub unsafe fn read_prop(
    ce: *mut zend_class_entry,
    obj: *mut zend_object,
    name: &CStr,
    rv: &mut zval,
) -> *mut zval {
    unsafe {
        deref(zend_read_property(
            ce,
            obj,
            name.as_ptr(),
            name.count_bytes(),
            true,
            rv,
        ))
    }
}

// HASH_KEY_IS_STRING is a #define on 8.4 and an enum constant on 8.5, so it is hardcoded and compared through i64::from in `Entries::next`.
const HASH_KEY_IS_STRING: i64 = 1;

/// The key of one HashTable entry. `Str` is the engine-owned key string and lives as long as the table.
pub enum Key {
    Str(*mut zend_string),
    Index(zend_ulong),
}

/// A walk over the entries of a HashTable, with each value dereferenced. It has no Drop glue, so a Zend bailout can longjmp through a frame that holds it. The position moves before the caller gets the entry, so the caller must not change the walked table.
pub struct Entries {
    ht: *mut HashTable,
    pos: HashPosition,
}

/// # Safety
/// `ht` a live array that outlives the walk.
pub unsafe fn entries(ht: *mut HashTable) -> Entries {
    let mut pos: HashPosition = 0;
    unsafe { zend_hash_internal_pointer_reset_ex(ht, &mut pos) };
    Entries { ht, pos }
}

impl Iterator for Entries {
    type Item = (Key, *mut zval);

    /// `&raw mut self.pos`: the pos parameter is *mut on PHP 8.4 and *const on 8.5.
    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            let entry = zend_hash_get_current_data_ex(self.ht, &raw mut self.pos);
            if entry.is_null() {
                return None;
            }
            let mut skey: *mut zend_string = std::ptr::null_mut();
            let mut nkey: zend_ulong = 0;
            let kt = zend_hash_get_current_key_ex(self.ht, &mut skey, &mut nkey, &self.pos);
            zend_hash_move_forward_ex(self.ht, &mut self.pos);
            let key = if i64::from(kt) == HASH_KEY_IS_STRING {
                Key::Str(skey)
            } else {
                Key::Index(nkey)
            };
            Some((key, deref(entry)))
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
