//! The Rust behind the PHP classes of the grpc plugin: the class entries and `DISPATCHER_CLASSES`.

use rapira_sapi::work::DispatcherClasses;
use rapira_sapi::zend_class_entry;

pub mod call;
mod values;

pub(crate) use call::GrpcState;

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
    pub static mut rapira_ce_internal_grpc_dispatcher: *mut zend_class_entry;
    pub static mut rapira_ce_internal_grpc_dispatcher_info: *mut zend_class_entry;
    pub static mut rapira_ce_internal_grpc_unary_call: *mut zend_class_entry;
    pub fn rapira_grpc_register_classes();
}

pub static DISPATCHER_CLASSES: DispatcherClasses = DispatcherClasses {
    dispatcher: || unsafe { rapira_ce_internal_grpc_dispatcher },
    info: || unsafe { rapira_ce_internal_grpc_dispatcher_info },
    unit: || unsafe { rapira_ce_internal_grpc_unary_call },
    busy: c"this worker holds one unit at a time, and its Rapira\\Grpc\\UnaryCall is unfinalized; finalize it first",
    quiesce: None,
};
