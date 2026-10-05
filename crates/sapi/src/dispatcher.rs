use std::ffi::{CStr, c_char, c_int};

use tracing::{Level, level_filters::LevelFilter};

use crate::{
    HashTable, IS_OBJECT, PHP_JSON_PARTIAL_OUTPUT_ON_ERROR, add_assoc_stringl_ex,
    add_assoc_zval_ex, callbacks::guard, php_json_encode, rapira_array_init, rapira_smart_str_free,
    smart_str, zend, zend_ce_throwable, zend_class_entry, zend_get_exception_base,
    zend_hash_index_update, zend_object, zend_string, zval, zval_add_ref, zval_ptr_dtor,
};

fn emit(level: Level, message: &[u8], context: &[u8]) {
    let message = String::from_utf8_lossy(message);
    let context = String::from_utf8_lossy(context);
    if context.is_empty() {
        crate::diagnostics::event_at!("app", level, "{message}");
    } else {
        crate::diagnostics::event_at!("app", level, context = %context, "{message}");
    }
}

/// An enum case's name is its first property slot (`zend_enum_fetch_case_name`, Zend/zend_enum.h).
unsafe fn level_from_case(level: *mut zend_object) -> Level {
    unsafe {
        let name_zv = (*level).properties_table.as_ptr();
        match zend::zstr_bytes((*name_zv).value.str_) {
            b"Error" => Level::ERROR,
            b"Warning" => Level::WARN,
            b"Info" => Level::INFO,
            b"Debug" => Level::DEBUG,
            b"Trace" => Level::TRACE,
            _ => Level::ERROR,
        }
    }
}

unsafe fn add_prop(
    dst: *mut zval,
    scope: *mut zend_class_entry,
    ex: *mut zend_object,
    name: &CStr,
) {
    unsafe {
        let mut rv: zval = std::mem::zeroed();
        let src = zend::read_prop(scope, ex, name, &mut rv);
        let mut copy: zval = *src;
        zval_add_ref(&mut copy);
        add_assoc_zval_ex(dst, name.as_ptr(), name.count_bytes(), &mut copy);
    }
}

/// Throwable state lives in private props, invisible to json_encode: flatten it explicitly.
unsafe fn flatten_throwable(dst: *mut zval, ex: *mut zend_object, depth: i32) {
    unsafe {
        rapira_array_init(dst, 6);
        let name = zend::zstr_bytes((*(*ex).ce).name);
        add_assoc_stringl_ex(
            dst,
            c"class".as_ptr(),
            c"class".count_bytes(),
            name.as_ptr().cast::<c_char>(),
            name.len(),
        );
        let base = zend_get_exception_base(ex);
        for prop in [c"message", c"code", c"file", c"line"] {
            add_prop(dst, base, ex, prop);
        }
        if depth <= 0 {
            return;
        }
        let mut rv: zval = std::mem::zeroed();
        let prev = zend::read_prop(base, ex, c"previous", &mut rv);
        if zend::zval_type(prev) == IS_OBJECT {
            let mut flat: zval = std::mem::zeroed();
            flatten_throwable(&mut flat, (*prev).value.obj, depth - 1);
            add_assoc_zval_ex(
                dst,
                c"previous".as_ptr(),
                c"previous".count_bytes(),
                &mut flat,
            );
        }
    }
}

/// A Zend bailout longjmps through this frame, so it holds no value with Drop glue: the returned Vec is created after the last bailing call.
unsafe fn context_json(context: *mut HashTable) -> Vec<u8> {
    unsafe {
        let mut rebuilt: zval = std::mem::zeroed();
        rapira_array_init(&mut rebuilt, (*context).nNumOfElements);
        for (key, entry) in zend::entries(context) {
            let mut out: zval = std::mem::zeroed();
            if zend::zval_type(entry) == IS_OBJECT
                && zend::instanceof((*(*entry).value.obj).ce, zend_ce_throwable)
            {
                flatten_throwable(&mut out, (*entry).value.obj, 4);
            } else {
                out = *entry;
                zval_add_ref(&mut out);
            }
            match key {
                zend::Key::Str(skey) => {
                    let kb = zend::zstr_bytes(skey);
                    add_assoc_zval_ex(&mut rebuilt, kb.as_ptr().cast(), kb.len(), &mut out);
                }
                zend::Key::Index(n) => {
                    zend_hash_index_update(rebuilt.value.arr, n, &mut out);
                }
            }
        }

        let mut buf: smart_str = std::mem::zeroed();
        let _ = php_json_encode(
            &mut buf,
            &mut rebuilt,
            PHP_JSON_PARTIAL_OUTPUT_ON_ERROR as c_int,
        );
        let json = if buf.s.is_null() {
            Vec::new()
        } else {
            zend::zstr_bytes(buf.s).to_vec()
        };
        rapira_smart_str_free(&mut buf);
        zval_ptr_dtor(&mut rebuilt);
        json
    }
}

/// # Safety
/// `message` a live zend_string; `level` NULL or a LogLevel case; `context` NULL or a live array, all ZPP-owned for the call with the engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_log_call(
    message: *mut zend_string,
    level: *mut zend_object,
    context: *mut HashTable,
) {
    guard((), || unsafe {
        let lvl = if level.is_null() {
            Level::INFO
        } else {
            level_from_case(level)
        };
        // A level above every enabled level skips the context encode and any user jsonSerialize().
        if lvl > LevelFilter::current() {
            return;
        }
        let json = if context.is_null() || (*context).nNumOfElements == 0 {
            Vec::new()
        } else {
            context_json(context)
        };
        emit(lvl, zend::zstr_bytes(message), &json);
    })
}
