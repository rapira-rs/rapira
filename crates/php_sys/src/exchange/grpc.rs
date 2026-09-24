use std::cell::RefCell;

use super::*;
use crate::{
    rapira_ce_grpc_method_info, rapira_ce_grpc_method_kind, rapira_ce_grpc_service_info,
    rapira_zval_enum_case,
    types::{GrpcMethod, GrpcService},
};

thread_local! {
    /// Set on a PHP thread that serves a gRPC pool.
    static SERVICES: RefCell<Option<Vec<GrpcService>>> = const { RefCell::new(None) };
}

/// Makes this PHP thread serve a gRPC pool with `services`.
pub(crate) fn serve_grpc(services: Vec<GrpcService>) {
    SERVICES.set(Some(services));
}

pub(crate) fn serving() -> bool {
    SERVICES.with_borrow(Option::is_some)
}

/// The MethodKind case name: client streaming streams the request, server streaming streams the response.
fn kind_case(client_streaming: bool, server_streaming: bool) -> &'static CStr {
    match (client_streaming, server_streaming) {
        (false, false) => c"Unary",
        (false, true) => c"ServerStreaming",
        (true, false) => c"ClientStreaming",
        (true, true) => c"BidiStreaming",
    }
}

/// # Safety
/// `dst` writable; engine active on this thread.
unsafe fn method_info(dst: *mut zval, m: &GrpcMethod) {
    unsafe {
        let ce = rapira_ce_grpc_method_info;
        let _ = object_init_ex(dst, ce);
        let obj = (*dst).value.obj;
        zend::prop_stringl(ce, obj, c"name", m.name.as_bytes());
        zend::prop_stringl(ce, obj, c"inputType", m.input_type.as_bytes());
        zend::prop_stringl(ce, obj, c"outputType", m.output_type.as_bytes());
        let mut kind: zval = std::mem::zeroed();
        rapira_zval_enum_case(
            &mut kind,
            rapira_ce_grpc_method_kind,
            kind_case(m.client_streaming, m.server_streaming).as_ptr(),
        );
        zend::prop_zval(ce, obj, c"kind", &mut kind);
        zval_ptr_dtor(&mut kind);
    }
}

/// `getServices()`: a new `list<ServiceInfo>` on each call.
/// # Safety
/// `rv` writable; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_services(rv: *mut zval) -> bool {
    guard(false, || unsafe {
        SERVICES.with_borrow(|services| {
            let services = services.as_deref().unwrap_or_default();
            rapira_array_init(rv, services.len() as u32);
            for s in services {
                let mut methods: zval = std::mem::zeroed();
                rapira_array_init(&mut methods, s.methods.len() as u32);
                for m in &s.methods {
                    let mut info: zval = std::mem::zeroed();
                    method_info(&mut info, m);
                    let _ = add_next_index_object(&mut methods, info.value.obj);
                }
                let ce = rapira_ce_grpc_service_info;
                let mut service: zval = std::mem::zeroed();
                let _ = object_init_ex(&mut service, ce);
                zend::prop_stringl(ce, service.value.obj, c"name", s.name.as_bytes());
                zend::prop_zval(ce, service.value.obj, c"methods", &mut methods);
                zval_ptr_dtor(&mut methods);
                let _ = add_next_index_object(rv, service.value.obj);
            }
        });
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        client_streaming: bool,
        server_streaming: bool,
        expected: &'static CStr,
    }

    // Expected values: the MethodKind cases of the Rapira\Grpc contract and the two streaming flags of a protobuf MethodDescriptorProto.
    const CASES: &[Case] = &[
        Case {
            name: "no streaming is unary",
            client_streaming: false,
            server_streaming: false,
            expected: c"Unary",
        },
        Case {
            name: "a streamed response is server streaming",
            client_streaming: false,
            server_streaming: true,
            expected: c"ServerStreaming",
        },
        Case {
            name: "a streamed request is client streaming",
            client_streaming: true,
            server_streaming: false,
            expected: c"ClientStreaming",
        },
        Case {
            name: "both streamed is bidi streaming",
            client_streaming: true,
            server_streaming: true,
            expected: c"BidiStreaming",
        },
    ];

    #[test]
    fn method_kind_follows_the_streaming_flags() {
        for c in CASES {
            assert_eq!(
                kind_case(c.client_streaming, c.server_streaming),
                c.expected,
                "{}",
                c.name
            );
        }
    }
}
