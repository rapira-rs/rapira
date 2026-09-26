use std::ffi::{CStr, c_void};

use rapira_sapi::plugin::PhpPart;
use rapira_sapi::work::DispatcherClasses;
use rapira_sapi::{zend_class_entry, zend_object, zval};

use crate::MethodInfo;

mod call;
mod values;

pub(crate) use call::{GrpcState, grpc_call_from};

// Class entries of the grpc stub; rapira_grpc_register_classes assigns them in MINIT (rapira_grpc.h).
unsafe extern "C" {
    pub static mut rapira_ce_grpc_method_kind: *mut zend_class_entry;
    pub static mut rapira_ce_grpc_protocol: *mut zend_class_entry;
    pub static mut rapira_ce_grpc_status: *mut zend_class_entry;
    pub static mut rapira_ce_grpc_metadata: *mut zend_class_entry;
    pub static mut rapira_ce_grpc_error_detail: *mut zend_class_entry;
    pub static mut rapira_ce_grpc_method_info: *mut zend_class_entry;
    pub static mut rapira_ce_grpc_service_info: *mut zend_class_entry;
    pub static mut rapira_ce_grpc_context: *mut zend_class_entry;
    pub static mut rapira_ce_grpc_exception: *mut zend_class_entry;
    pub static mut rapira_ce_internal_grpc_dispatcher: *mut zend_class_entry;
    pub static mut rapira_ce_internal_grpc_dispatcher_info: *mut zend_class_entry;
    pub static mut rapira_ce_internal_grpc_unary_call: *mut zend_class_entry;
    pub static mut rapira_ce_internal_grpc_response_metadata: *mut zend_class_entry;
    fn rapira_grpc_register_classes();
}

/// Mirrors `rapira_grpc_call_obj` in rapira_grpc.h. The C fields sit before `std`.
#[repr(C)]
pub struct CallObj {
    pub state: *mut c_void,
    pub message: zval,
    pub context: zval,
    pub metadata: zval,
    pub std: zend_object,
}

/// Mirrors `rapira_grpc_metadata_obj` in rapira_grpc.h. The C fields sit before `std`.
#[repr(C)]
pub struct MetadataObj {
    pub state: *mut c_void,
    pub std: zend_object,
}

pub static DISPATCHER_CLASSES: DispatcherClasses = DispatcherClasses {
    dispatcher: || unsafe { rapira_ce_internal_grpc_dispatcher },
    info: || unsafe { rapira_ce_internal_grpc_dispatcher_info },
    unit: || unsafe { rapira_ce_internal_grpc_unary_call },
    busy: c"receive() while a Rapira\\Grpc\\UnaryCall is unfinalized; finalize it first",
};

pub static PHP_PART: PhpPart = PhpPart {
    register: rapira_grpc_register_classes,
    dispatcher: Some(DISPATCHER_CLASSES),
};

/// `Rapira\Grpc\MethodKind`: client streaming streams the request, server streaming streams the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodKind {
    Unary,
    ServerStreaming,
    ClientStreaming,
    BidiStreaming,
}

impl MethodKind {
    pub(crate) fn of(m: &MethodInfo) -> Self {
        match (m.client_streaming, m.server_streaming) {
            (false, false) => Self::Unary,
            (false, true) => Self::ServerStreaming,
            (true, false) => Self::ClientStreaming,
            (true, true) => Self::BidiStreaming,
        }
    }

    /// From the backing value of a case.
    pub(crate) fn from_value(value: &[u8]) -> Option<Self> {
        Some(match value {
            b"unary" => Self::Unary,
            b"server-streaming" => Self::ServerStreaming,
            b"client-streaming" => Self::ClientStreaming,
            b"bidi-streaming" => Self::BidiStreaming,
            _ => return None,
        })
    }

    pub(crate) fn case(self) -> &'static CStr {
        match self {
            Self::Unary => c"Unary",
            Self::ServerStreaming => c"ServerStreaming",
            Self::ClientStreaming => c"ClientStreaming",
            Self::BidiStreaming => c"BidiStreaming",
        }
    }

    pub(crate) fn streams_request(self) -> bool {
        matches!(self, Self::ClientStreaming | Self::BidiStreaming)
    }

    pub(crate) fn streams_response(self) -> bool {
        matches!(self, Self::ServerStreaming | Self::BidiStreaming)
    }
}
