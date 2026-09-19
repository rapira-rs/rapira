use std::ffi::CStr;

use crate::{callbacks::guard, zend, *};

unsafe fn object_list_valid(
    table: *mut HashTable,
    class: *mut zend_class_entry,
    argument: u32,
    message: &CStr,
) -> bool {
    if table.is_null() {
        return true;
    }
    unsafe {
        let mut position: HashPosition = 0;
        let mut expected = 0;
        zend_hash_internal_pointer_reset_ex(table, &mut position);
        loop {
            let value = zend_hash_get_current_data_ex(table, &raw mut position);
            if value.is_null() {
                return true;
            }
            let mut key = std::ptr::null_mut();
            let mut index = 0;
            let kind = zend_hash_get_current_key_ex(table, &mut key, &mut index, &position);
            let value = zend::deref(value);
            if i64::from(kind) == HASH_KEY_IS_STRING
                || index != expected
                || zend::zval_type(value) != IS_OBJECT
                || !zend::instanceof((*(*value).value.obj).ce, class)
            {
                zend_argument_type_error(argument, c"%s".as_ptr(), message.as_ptr());
                return false;
            }
            expected += 1;
            zend_hash_move_forward_ex(table, &mut position);
        }
    }
}

// Copy dereferenced objects so caller-owned references cannot change a readonly list.
unsafe fn object_list_property(
    class: *mut zend_class_entry,
    object: *mut zend_object,
    property: &CStr,
    source: *mut HashTable,
) {
    unsafe {
        let mut list: zval = std::mem::zeroed();
        rapira_array_init(&mut list, 0);
        if !source.is_null() {
            let mut position: HashPosition = 0;
            zend_hash_internal_pointer_reset_ex(source, &mut position);
            loop {
                let value = zend_hash_get_current_data_ex(source, &raw mut position);
                if value.is_null() {
                    break;
                }
                let value = zend::deref(value);
                zval_add_ref(value);
                add_next_index_object(&mut list, (*value).value.obj);
                zend_hash_move_forward_ex(source, &mut position);
            }
        }
        zend::prop_zval(class, object, property, &mut list);
        zval_ptr_dtor(&mut list);
    }
}

unsafe fn status_properties(
    object: *mut zend_object,
    code: *mut zval,
    message: *mut zend_string,
    details: *mut HashTable,
) {
    unsafe {
        let class = rapira_ce_grpc_status;
        zend::prop_zval(class, object, c"code", code);
        if message.is_null() {
            zend::prop_stringl(class, object, c"message", b"");
        } else {
            zend::prop_zstr(class, object, c"message", message);
        }
        object_list_property(class, object, c"details", details);
    }
}

/// # Safety
/// The engine is active. Arguments are borrowed from ZPP for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_ctor_error_detail(
    object: *mut zend_object,
    type_url: *mut zend_string,
    value: *mut zend_string,
) -> bool {
    guard(false, || unsafe {
        let class = rapira_ce_grpc_error_detail;
        zend::prop_zstr(class, object, c"typeUrl", type_url);
        zend::prop_zstr(class, object, c"value", value);
        !zend::exception_pending()
    })
}

/// # Safety
/// The engine is active. ZPP validated the status-code type. Optional pointers can be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_ctor_status(
    object: *mut zend_object,
    code: *mut zval,
    message: *mut zend_string,
    details: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        if !object_list_valid(
            details,
            rapira_ce_grpc_error_detail,
            3,
            c"must be a list of Rapira\\Grpc\\ErrorDetail objects",
        ) {
            return false;
        }
        status_properties(object, code, message, details);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_grpc_ctor_status`; the object is a GrpcException instance.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_ctor_exception(
    object: *mut zend_object,
    code: *mut zval,
    message: *mut zend_string,
    details: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        if !object_list_valid(
            details,
            rapira_ce_grpc_error_detail,
            3,
            c"must be a list of Rapira\\Grpc\\ErrorDetail objects",
        ) {
            return false;
        }
        let mut status: zval = std::mem::zeroed();
        object_init_ex(&mut status, rapira_ce_grpc_status);
        status_properties(status.value.obj, code, message, details);
        zend::prop_zval(rapira_ce_grpc_exception, object, c"status", &mut status);
        zval_ptr_dtor(&mut status);
        if message.is_null() {
            zend::prop_stringl(zend_get_exception_base(object), object, c"message", b"");
        } else {
            zend::prop_zstr(zend_get_exception_base(object), object, c"message", message);
        }
        !zend::exception_pending()
    })
}

/// # Safety
/// The engine is active. Arguments are ZPP-owned and kind has the MethodKind type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_ctor_method(
    object: *mut zend_object,
    name: *mut zend_string,
    input: *mut zend_string,
    output: *mut zend_string,
    kind: *mut zval,
) -> bool {
    guard(false, || unsafe {
        let class = rapira_ce_grpc_method_info;
        zend::prop_zstr(class, object, c"name", name);
        zend::prop_zstr(class, object, c"inputType", input);
        zend::prop_zstr(class, object, c"outputType", output);
        zend::prop_zval(class, object, c"kind", kind);
        !zend::exception_pending()
    })
}

/// # Safety
/// The engine is active. Arguments are borrowed from ZPP for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_ctor_service(
    object: *mut zend_object,
    name: *mut zend_string,
    methods: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        if !object_list_valid(
            methods,
            rapira_ce_grpc_method_info,
            2,
            c"must be a list of Rapira\\Grpc\\MethodInfo objects",
        ) {
            return false;
        }
        let class = rapira_ce_grpc_service_info;
        zend::prop_zstr(class, object, c"name", name);
        object_list_property(class, object, c"methods", methods);
        !zend::exception_pending()
    })
}

/// # Safety
/// `kind` is the borrowed backing string of a MethodKind enum case.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_method_streaming(
    kind: *mut zend_string,
    request: bool,
) -> bool {
    let bytes = unsafe { zend::zstr_bytes(kind) };
    bytes == b"bidi-streaming"
        || (request && bytes == b"client-streaming")
        || (!request && bytes == b"server-streaming")
}
