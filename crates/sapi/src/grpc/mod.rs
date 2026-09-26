mod call;

pub use call::Call;

use std::cell::RefCell;

use crate::types::GrpcService;
use crate::work::DispatcherClasses;

pub static DISPATCHER_CLASSES: DispatcherClasses = DispatcherClasses {
    dispatcher: || unsafe { crate::rapira_ce_internal_grpc_dispatcher },
    info: || unsafe { crate::rapira_ce_internal_grpc_dispatcher_info },
    unit: || unsafe { crate::rapira_ce_internal_grpc_unary_call },
    busy: c"receive() while a Rapira\\Grpc\\UnaryCall is unfinalized; finalize it first",
};

thread_local! {
    /// The services of the gRPC pool this PHP thread serves.
    pub(crate) static SERVICES: RefCell<Option<Vec<GrpcService>>> = const { RefCell::new(None) };
}

/// Sets the services `getServices()` reports on this PHP thread.
pub fn set_services(services: Vec<GrpcService>) {
    SERVICES.set(Some(services));
}
