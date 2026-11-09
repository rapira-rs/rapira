//! The Rust behind the PHP classes of the grpc plugin: the class entries and the dispatcher record.

use rapira_sapi::{rapira_plugin, zend_class_entry};

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
    // The dispatcher surface of the grpc plugin (rapira_grpc_classes.c).
    pub static mut rapira_grpc_plugin: rapira_plugin;
    pub fn rapira_grpc_register_classes();
}
