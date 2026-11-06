//! The constructor of the value class Metadata: its rules share `call::printable` and `call::is_binary` with the protocol code.

use std::ffi::c_int;

use rapira_sapi::callbacks::guard;
use rapira_sapi::{
    HashTable, IS_ARRAY, IS_STRING, rapira_array_is_list, zend, zend_object, zend_type_error,
    zend_value_error, zval,
};

/// Throws the contract's TypeError for `key`.
/// # Safety
/// Engine active on this thread.
unsafe fn not_a_list(key: &[u8]) {
    unsafe {
        zend_type_error(
            c"metadata key %.*s must map to a list of strings".as_ptr(),
            key.len() as c_int,
            key.as_ptr(),
        );
    }
}

/// Throws and returns false on the first entry that breaks the Metadata rules, in the order of the contract's constructor body: the key is non-empty lower-case ASCII, it maps to a list, and each value is a string, printable ASCII (0x20-0x7E) under a key without the `-bin` suffix.
/// A canonical decimal key such as "123" is an int key in a PHP array; its decimal text passes the key rule and has no `-bin` suffix.
/// # Safety
/// `ht` a live array; entries stay ZPP-owned.
unsafe fn metadata_valid(ht: *mut HashTable) -> bool {
    unsafe {
        for zend::Entry { key, val: list } in zend::entries(ht) {
            let num_text;
            let key = match key {
                zend::Key::Str(s) => zend::zstr_bytes(s),
                zend::Key::Index(n) => {
                    num_text = (n as i64).to_string();
                    num_text.as_bytes()
                }
            };
            if key.is_empty() || !key.iter().all(|b| b.is_ascii() && !b.is_ascii_uppercase()) {
                zend::throw_value_error(c"a metadata key must be non-empty lower-case ASCII");
                return false;
            }
            if zend::zval_type(list) != IS_ARRAY || !rapira_array_is_list((*list).value.arr) {
                not_a_list(key);
                return false;
            }
            let binary = super::call::is_binary(key);
            for zend::Entry { val: item, .. } in zend::entries((*list).value.arr) {
                if zend::zval_type(item) != IS_STRING {
                    not_a_list(key);
                    return false;
                }
                if !binary && !super::call::printable(zend::zstr_bytes((*item).value.str_)) {
                    zend_value_error(
                        c"a value of metadata key %.*s is not printable ASCII".as_ptr(),
                        key.len() as c_int,
                        key.as_ptr(),
                    );
                    return false;
                }
            }
        }
        true
    }
}

/// # Safety
/// `obj` is under construction; `entries` a live array zval, ZPP-owned for the call.
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
