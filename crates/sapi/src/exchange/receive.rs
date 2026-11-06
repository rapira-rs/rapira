use super::*;

const CLOSED: &std::ffi::CStr = c"no more work for this app instance";

enum RecvMode {
    Wait(i64),
    Try,
}

/// Allocates the wrapper object before pulling: a unit pulled with no owner would leak its Frame sender and hang the client.
/// # Safety
/// `return_value` writable; engine active on this thread.
unsafe fn receive_into(return_value: *mut zval, mode: RecvMode) -> bool {
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
            // SAFETY: worker_main points it at the 'static busy message of the plugin.
            zend::throw_error(std::ffi::CStr::from_ptr(crate::rapira_plugin_busy));
            return false;
        }
        let mut obj: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut obj, crate::rapira_plugin_ce_unit);
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
                    // SAFETY: obj is a live object of rapira_plugin_ce_unit, allocated above on this thread.
                    (*HeldPtr::of(obj.value.obj)).0 = Some(ptr);
                    UNIT.set(Some(ptr.as_ptr()));
                    *return_value = obj;
                    return true;
                }
                _ if armed => {
                    armed = false;
                    // SAFETY: plain zend timer bookkeeping; no bailout path.
                    rapira_receive_untimed();
                }
                // The app instance takes no more units: it sees the source closed, and the worker stays.
                Pull::Closed => {
                    zval_ptr_dtor(&mut obj);
                    zend::throw_exception(rapira_ce_closed_exception, CLOSED);
                    return false;
                }
                Pull::NoUnit if matches!(mode, RecvMode::Try) => {
                    zval_ptr_dtor(&mut obj);
                    (*return_value).u1.type_info = IS_NULL;
                    return true;
                }
                Pull::NoUnit => {
                    zval_ptr_dtor(&mut obj);
                    zend::throw_exception(
                        rapira_ce_timeout_exception,
                        c"no work became available within the timeout",
                    );
                    return false;
                }
            }
        }
    }
}

/// # Safety
/// `timeout_us >= -1`; `return_value` writable; engine active on this thread (the receive verbs touch the zend timer).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_receive(timeout_us: i64, return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        receive_into(return_value, RecvMode::Wait(timeout_us))
    })
}

/// # Safety
/// As `rapira_rs_receive`; never blocks. Empty writes null instead of throwing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_try_receive(return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        receive_into(return_value, RecvMode::Try)
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
