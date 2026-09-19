use std::ffi::c_void;

use super::{
    ServiceInfo,
    call::{CallState, SERVICES},
};
use crate::{
    callbacks::guard,
    exchange::request::{build_address, build_tls},
    types::FieldLines,
    zend, *,
};

// Only borrowed Rust data and plain zvals cross allocating Zend calls.
unsafe fn metadata(out: *mut zval, fields: &FieldLines) {
    unsafe {
        let mut entries: zval = std::mem::zeroed();
        rapira_array_init(&mut entries, 0);
        for (index, (name, _)) in fields.iter().enumerate() {
            if fields[..index].iter().any(|(prior, _)| prior == name) {
                continue;
            }
            let mut values: zval = std::mem::zeroed();
            rapira_array_init(&mut values, 0);
            for (_, value) in fields.iter().filter(|(key, _)| key == name) {
                zend::list_push_stringl(&mut values, value);
            }
            zend_hash_str_update(
                entries.value.arr,
                name.as_ptr().cast(),
                name.len(),
                &mut values,
            );
        }
        object_init_ex(out, rapira_ce_grpc_metadata);
        zend::prop_zval(
            rapira_ce_grpc_metadata,
            (*out).value.obj,
            c"entries",
            &mut entries,
        );
        zval_ptr_dtor(&mut entries);
    }
}

unsafe extern "C" fn metadata_builder(data: *const c_void, out: *mut zval) {
    unsafe {
        metadata(out, &*data.cast::<FieldLines>());
    }
}

/// # Safety
/// State belongs to a live native metadata object. Output is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_metadata_snapshot(
    ptr: *mut c_void,
    trailer: bool,
    out: *mut zval,
) -> bool {
    guard(false, || unsafe {
        let state = &*ptr.cast::<CallState>();
        let fields = if trailer {
            state.trailers.clone()
        } else {
            state.headers.clone()
        };
        rapira_grpc_build(Some(metadata_builder), (&raw const fields).cast(), out)
    })
}

unsafe extern "C" fn context_builder(data: *const c_void, out: *mut zval) {
    unsafe {
        let state = &*data.cast::<CallState>();
        let request = &state.request;
        let ce = rapira_ce_grpc_context;
        object_init_ex(out, ce);
        let obj = (*out).value.obj;
        zend::prop_stringl(ce, obj, c"method", request.method.as_bytes());
        let mut value: zval = std::mem::zeroed();
        metadata(&mut value, &request.metadata);
        zend::prop_zval(ce, obj, c"metadata", &mut value);
        zval_ptr_dtor(&mut value);
        match request.deadline {
            Some(deadline) => zend::prop_double(ce, obj, c"deadline", deadline),
            None => zend::prop_null(ce, obj, c"deadline"),
        }
        build_address(&mut value, &state.remote);
        zend::prop_zval(ce, obj, c"remote", &mut value);
        zval_ptr_dtor(&mut value);
        if let Some(tls) = &request.tls {
            build_tls(&mut value, tls);
            zend::prop_zval(ce, obj, c"tls", &mut value);
            zval_ptr_dtor(&mut value);
        } else {
            zend::prop_null(ce, obj, c"tls");
        }
        rapira_enum_case(rapira_ce_grpc_protocol, c"Grpc".as_ptr(), &mut value);
        zend::prop_zval(ce, obj, c"protocol", &mut value);
        zval_ptr_dtor(&mut value);
        zend::prop_double(ce, obj, c"receivedAt", request.received_at);
    }
}

/// # Safety
/// Call is live and initialized. Output is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_context(
    call: *mut rapira_grpc_call_obj,
    out: *mut zval,
) -> bool {
    guard(false, || unsafe {
        if zend::is_undef(&(*call).context)
            && !rapira_grpc_build(
                Some(context_builder),
                (*call).state,
                &raw mut (*call).context,
            )
        {
            return false;
        }
        *out = (*call).context;
        zval_add_ref(out);
        true
    })
}

unsafe extern "C" fn services_builder(data: *const c_void, out: *mut zval) {
    unsafe {
        let services = &*data.cast::<Vec<ServiceInfo>>();
        rapira_array_init(out, services.len() as u32);
        for service in services {
            let mut methods: zval = std::mem::zeroed();
            rapira_array_init(&mut methods, service.methods.len() as u32);
            for method in &service.methods {
                let ce = rapira_ce_grpc_method_info;
                let mut value: zval = std::mem::zeroed();
                object_init_ex(&mut value, ce);
                let obj = value.value.obj;
                zend::prop_stringl(ce, obj, c"name", method.name.as_bytes());
                zend::prop_stringl(ce, obj, c"inputType", method.input_type.as_bytes());
                zend::prop_stringl(ce, obj, c"outputType", method.output_type.as_bytes());
                let mut kind: zval = std::mem::zeroed();
                rapira_enum_case(rapira_ce_grpc_method_kind, c"Unary".as_ptr(), &mut kind);
                zend::prop_zval(ce, obj, c"kind", &mut kind);
                zval_ptr_dtor(&mut kind);
                add_next_index_object(&mut methods, obj);
            }
            let ce = rapira_ce_grpc_service_info;
            let mut value: zval = std::mem::zeroed();
            object_init_ex(&mut value, ce);
            zend::prop_stringl(ce, value.value.obj, c"name", service.name.as_bytes());
            zend::prop_zval(ce, value.value.obj, c"methods", &mut methods);
            zval_ptr_dtor(&mut methods);
            add_next_index_object(out, value.value.obj);
        }
    }
}

/// # Safety
/// The engine is active and output is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_services(out: *mut zval) -> bool {
    guard(false, || unsafe {
        SERVICES.with_borrow(|services| {
            let empty = Vec::new();
            let services = services.as_ref().unwrap_or(&empty);
            rapira_grpc_build(Some(services_builder), (&raw const *services).cast(), out)
        })
    })
}

/// # Safety
/// Arguments are borrowed from ZPP. The engine is active.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn rapira_rs_grpc_ctor_context(
    obj: *mut zend_object,
    method: *mut zend_string,
    metadata: *mut zval,
    deadline: f64,
    deadline_null: bool,
    remote: *mut zval,
    tls: *mut zval,
    protocol: *mut zval,
    received_at: f64,
) -> bool {
    guard(false, || unsafe {
        if !crate::values::address_arg(remote, 4) {
            return false;
        }
        let ce = rapira_ce_grpc_context;
        zend::prop_zstr(ce, obj, c"method", method);
        zend::prop_zval(ce, obj, c"metadata", metadata);
        if deadline_null {
            zend::prop_null(ce, obj, c"deadline");
        } else {
            zend::prop_double(ce, obj, c"deadline", deadline);
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
