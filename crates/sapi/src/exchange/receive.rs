use super::*;
use crate::zend_class_entry;

const CLOSED: &std::ffi::CStr = c"no more work for this app instance";

enum RecvMode {
    Wait(i64),
    Try,
}

/// Pulls a unit, writes it to `held` and to the cycle slot, and returns its class. Null when no unit arrives.
/// The cycle slot owns the unit until the C shell stores the held word in the object: a unit with no owner would leak its Frame sender and hang the client.
/// # Safety
/// `held` is writable; engine active on this thread.
unsafe fn receive_into(held: *mut HeldPtr, mode: RecvMode) -> *mut zend_class_entry {
    unsafe {
        if let Some(ptr) = UNIT.get()
            && !(*ptr).settled()
            && (*ptr).cancelled()
        {
            tracing::debug!(target: "rapira", "receive() discarded an unfinalized unit whose client left");
            (*ptr).discard();
        }
        if let Some(ptr) = UNIT.get()
            && !(*ptr).settled()
        {
            // SAFETY: a unit exists only in dispatcher mode, where start_worker points rapira_plugin_current at the static record of the plugin, and the record holds a static busy text.
            zend::throw_error(std::ffi::CStr::from_ptr(
                (*crate::rapira_plugin_current).busy,
            ));
            return std::ptr::null_mut();
        }
        // The timer of the previous unit still runs. A queued unit re-arms it, so disarm it only before a wait.
        let mut armed = matches!(mode, RecvMode::Wait(t) if t != 0);
        if !armed {
            // SAFETY: plain zend timer bookkeeping on this thread; no bailout path.
            rapira_receive_untimed();
        }
        loop {
            let wait = match mode {
                RecvMode::Wait(_) if armed => Wait::Never,
                RecvMode::Try | RecvMode::Wait(0) => Wait::Never,
                RecvMode::Wait(-1) => Wait::Forever,
                RecvMode::Wait(t) => Wait::For(Duration::from_micros(t as u64)),
            };
            match pull(wait) {
                Pull::Unit(unit) => {
                    // SAFETY: plain zend timer bookkeeping; no bailout path.
                    rapira_receive_timed();
                    let ptr = NonNull::from(Box::leak(unit.into_held()));
                    held.write(HeldPtr(Some(ptr)));
                    UNIT.set(Some(ptr.as_ptr()));
                    return ptr.as_ref().class();
                }
                _ if armed => {
                    armed = false;
                    // SAFETY: plain zend timer bookkeeping; no bailout path.
                    rapira_receive_untimed();
                }
                // The app instance takes no more units: it sees the source closed, and the worker stays.
                Pull::Closed => {
                    zend::throw_exception(rapira_ce_closed_exception, CLOSED);
                    return std::ptr::null_mut();
                }
                Pull::NoUnit if matches!(mode, RecvMode::Try) => return std::ptr::null_mut(),
                Pull::NoUnit => {
                    zend::throw_exception(
                        rapira_ce_timeout_exception,
                        c"no work became available within the timeout",
                    );
                    return std::ptr::null_mut();
                }
            }
        }
    }
}

/// The class of the unit that `held` now holds. Null with an exception set: closed, busy or timeout. tryReceive: null with no exception means no unit is queued, and the shell returns null.
/// # Safety
/// `timeout_us >= -1`; `held` is writable; engine active on this thread (the receive verbs touch the zend timer).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_receive(
    timeout_us: i64,
    held: *mut HeldPtr,
) -> *mut zend_class_entry {
    guard(std::ptr::null_mut(), || unsafe {
        receive_into(held, RecvMode::Wait(timeout_us))
    })
}

/// As `rapira_rs_receive`; never blocks.
/// # Safety
/// As `rapira_rs_receive`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_try_receive(held: *mut HeldPtr) -> *mut zend_class_entry {
    guard(std::ptr::null_mut(), || unsafe {
        receive_into(held, RecvMode::Try)
    })
}

/// The intake depth and whether the unit handed out last is unfinalized: the two counts of a DispatcherInfo object.
/// # Safety
/// `pending` and `active` writable; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_dispatcher_counts(pending: *mut i64, active: *mut i64) {
    guard((), || unsafe {
        *pending = pending_depth() as i64;
        *active = i64::from(UNIT.get().is_some_and(|p| !(*p).is_finalized()));
    })
}
