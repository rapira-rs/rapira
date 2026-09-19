use crate::{callbacks::guard, zend, *};

pub(super) fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(b))
}

pub(super) fn valid_text(value: &[u8]) -> bool {
    value.iter().all(|b| (0x20..=0x7e).contains(b))
}

unsafe fn validate(entries: *mut HashTable) -> bool {
    if entries.is_null() {
        return true;
    }
    unsafe {
        let mut pos = 0;
        zend_hash_internal_pointer_reset_ex(entries, &mut pos);
        loop {
            let value = zend_hash_get_current_data_ex(entries, &raw mut pos);
            if value.is_null() {
                return true;
            }
            let mut key = std::ptr::null_mut();
            let mut index = 0;
            let kind = zend_hash_get_current_key_ex(entries, &mut key, &mut index, &pos);
            if i64::from(kind) != HASH_KEY_IS_STRING || !valid_name(zend::zstr_bytes(key)) {
                zend::throw_value_error(c"metadata names must be nonempty lowercase gRPC names");
                return false;
            }
            let binary = zend::zstr_bytes(key).ends_with(b"-bin");
            let value = zend::deref(value);
            if zend::zval_type(value) != IS_ARRAY {
                zend_argument_type_error(
                    1,
                    c"must map metadata names to lists of strings".as_ptr(),
                );
                return false;
            }
            let list = (*value).value.arr;
            let mut lp = 0;
            let mut expected = 0;
            zend_hash_internal_pointer_reset_ex(list, &mut lp);
            loop {
                let item = zend_hash_get_current_data_ex(list, &raw mut lp);
                if item.is_null() {
                    break;
                }
                let item = zend::deref(item);
                let kind = zend_hash_get_current_key_ex(list, &mut key, &mut index, &lp);
                if i64::from(kind) == HASH_KEY_IS_STRING
                    || index != expected
                    || zend::zval_type(item) != IS_STRING
                {
                    zend_argument_type_error(
                        1,
                        c"must map metadata names to lists of strings".as_ptr(),
                    );
                    return false;
                }
                if !binary && !valid_text(zend::zstr_bytes((*item).value.str_)) {
                    zend::throw_value_error(
                        c"text metadata values must contain printable ASCII bytes",
                    );
                    return false;
                }
                expected += 1;
                zend_hash_move_forward_ex(list, &mut lp);
            }
            zend_hash_move_forward_ex(entries, &mut pos);
        }
    }
}

/// # Safety
/// The engine is active. The table is borrowed from ZPP.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_metadata_construct(
    obj: *mut zend_object,
    entries: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        if !validate(entries) {
            return false;
        }
        let mut copy: zval = std::mem::zeroed();
        rapira_array_init(&mut copy, 0);
        if !entries.is_null() {
            let mut pos = 0;
            zend_hash_internal_pointer_reset_ex(entries, &mut pos);
            loop {
                let value = zend_hash_get_current_data_ex(entries, &raw mut pos);
                if value.is_null() {
                    break;
                }
                let mut key = std::ptr::null_mut();
                let mut index = 0;
                zend_hash_get_current_key_ex(entries, &mut key, &mut index, &pos);
                let table = (*zend::deref(value)).value.arr;
                let mut list: zval = std::mem::zeroed();
                rapira_array_init(&mut list, (*table).nNumOfElements);
                let mut lp = 0;
                zend_hash_internal_pointer_reset_ex(table, &mut lp);
                loop {
                    let item = zend_hash_get_current_data_ex(table, &raw mut lp);
                    if item.is_null() {
                        break;
                    }
                    zend::list_push_stringl(
                        &mut list,
                        zend::zstr_bytes((*zend::deref(item)).value.str_),
                    );
                    zend_hash_move_forward_ex(table, &mut lp);
                }
                zend_hash_str_update(copy.value.arr, (*key).val.as_ptr(), (*key).len, &mut list);
                zend_hash_move_forward_ex(entries, &mut pos);
            }
        }
        zend::prop_zval(rapira_ce_grpc_metadata, obj, c"entries", &mut copy);
        zval_ptr_dtor(&mut copy);
        !zend::exception_pending()
    })
}

unsafe fn entries(obj: *mut zend_object) -> Option<*mut zval> {
    unsafe {
        let value = zend_read_property(
            rapira_ce_grpc_metadata,
            obj,
            c"entries".as_ptr(),
            7,
            false,
            std::ptr::null_mut(),
        );
        (zend::zval_type(value) == IS_ARRAY).then_some(value)
    }
}

/// # Safety
/// The object is initialized Metadata. The name and output are live for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_metadata_values(
    obj: *mut zend_object,
    name: *mut zend_string,
    out: *mut zval,
) {
    guard((), || unsafe {
        let Some(entries) = entries(obj) else {
            return;
        };
        let table = (*entries).value.arr;
        let mut pos = 0;
        zend_hash_internal_pointer_reset_ex(table, &mut pos);
        loop {
            let value = zend_hash_get_current_data_ex(table, &raw mut pos);
            if value.is_null() {
                break;
            }
            let mut key = std::ptr::null_mut();
            let mut index = 0;
            zend_hash_get_current_key_ex(table, &mut key, &mut index, &pos);
            if zend::zstr_bytes(key).eq_ignore_ascii_case(zend::zstr_bytes(name)) {
                *out = *value;
                zval_add_ref(out);
                return;
            }
            zend_hash_move_forward_ex(table, &mut pos);
        }
        rapira_array_init(out, 0);
    });
}

/// # Safety
/// The object is initialized Metadata.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_metadata_count(obj: *mut zend_object) -> i64 {
    guard(0, || unsafe {
        entries(obj).map_or(0, |entries| {
            i64::from((*(*entries).value.arr).nNumOfElements)
        })
    })
}

/// # Safety
/// The object is initialized Metadata and output is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_metadata_iterator(obj: *mut zend_object, out: *mut zval) {
    guard((), || unsafe {
        if let Some(entries) = entries(obj) {
            rapira_array_iterator(out, entries);
        }
    });
}
