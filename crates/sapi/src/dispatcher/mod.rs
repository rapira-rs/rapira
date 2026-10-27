pub(crate) use std::{cell::Cell, ffi::c_int, time::Duration};

pub(crate) use crate::work::{DispatcherClasses, Held, release};
pub(crate) use crate::{
    IS_NULL, RAPIRA_MODE_DISPATCHER,
    callbacks::guard,
    object_init_ex, rapira_ce_closed_exception, rapira_ce_no_dispatcher_error,
    rapira_ce_timeout_exception, rapira_dispatcher_info_obj, rapira_receive_timed,
    rapira_receive_untimed,
    scoreboard::{Event, sb_update},
    start::{Pulled, pending_depth, pull_unit_try, pull_unit_wait},
    zend, zval, zval_add_ref, zval_ptr_dtor,
};

mod receive;

thread_local! {
    /// The dispatcher classes of the plugin this PHP thread serves; None outside dispatcher mode.
    static CLASSES: Cell<Option<DispatcherClasses>> = const { Cell::new(None) };
}

pub(crate) fn set_classes(classes: DispatcherClasses) {
    CLASSES.set(Some(classes));
}

fn classes() -> DispatcherClasses {
    CLASSES
        .get()
        .expect("a dispatcher-mode worker started with no DispatcherClasses")
}

thread_local! {
    /// The Box pointer of the unit handed out last, so paths where free_obj never runs (bailout) can still reclaim it.
    pub(crate) static UNIT: Cell<Option<*mut dyn Held>> = const { Cell::new(None) };
    pub(crate) static CLOSED_SEEN: Cell<bool> = const { Cell::new(false) };
    /// A unit was handed out this cycle: a fatal after that is an app failure, not a boot failure.
    pub(crate) static RECEIVED: Cell<bool> = const { Cell::new(false) };
}

/// Clears the cycle slot if it still points at `ptr`.
pub(crate) fn forget_held(ptr: *const ()) {
    if UNIT.get().is_some_and(|u| std::ptr::addr_eq(u, ptr)) {
        UNIT.set(None);
    }
}

pub(crate) fn cycle_reset() {
    reclaim_current();
    UNIT.set(None);
    CLOSED_SEEN.set(false);
    RECEIVED.set(false);
    // A bailout in RSHUTDOWN skips rapira_rs_dispatcher_release(). The request shutdown freed the object, so drop the zval without a dtor.
    receive::DISPATCHER.set(None);
}

/// Reclaim a unit free_obj never saw (shutdown bailout / allocation bailout).
pub(crate) fn reclaim_current() {
    if let Some(ptr) = UNIT.get() {
        // SAFETY: the pointer came from Box::into_raw in receive, and rapira_rs_exchange_drop clears the unit before it reclaims.
        drop(unsafe { release(ptr) });
    }
}

/// `fn $name(obj) -> *mut $t` recovers the enclosing C struct: the C fields sit before `std` (the layouts in rapira_sapi.h and the plugin headers).
#[macro_export]
macro_rules! container_of {
    ($vis:vis $name:ident, $t:ty) => {
        $vis unsafe fn $name(obj: *mut $crate::zend_object) -> *mut $t {
            unsafe { obj.byte_sub(std::mem::offset_of!($t, std)).cast() }
        }
    };
}

container_of!(info_from, rapira_dispatcher_info_obj);
