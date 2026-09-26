mod exchange;

pub use exchange::Exchange;

use crate::work::DispatcherClasses;

pub static DISPATCHER_CLASSES: DispatcherClasses = DispatcherClasses {
    dispatcher: || unsafe { crate::rapira_ce_internal_http_dispatcher },
    info: || unsafe { crate::rapira_ce_internal_http_dispatcher_info },
    unit: || unsafe { crate::rapira_ce_internal_http_exchange },
    busy: c"receive() while a Rapira\\Http\\Exchange is unfinalized; finalize it first",
};
