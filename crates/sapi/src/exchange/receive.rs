use super::*;

const CLOSED: &std::ffi::CStr = c"no more work for this app instance";

enum RecvMode {
    Wait(i64),
    Try,
}

/// Pulls a unit and creates its object in `rv`. The object has the class of the unit, and this function writes its held word. Returns false when no unit arrives.
/// The cycle slot owns the unit until the object holds it, because a unit with no owner would leak its Frame sender and hang the client.
/// # Safety
/// `rv` is the return value of the receive shell, and the engine is active on this thread.
unsafe fn receive_into(rv: *mut zval, mode: RecvMode) -> bool {
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
            // SAFETY: a dispatcher unit has a static plugin record and a static error message.
            zend::throw_error(std::ffi::CStr::from_ptr(
                (*crate::rapira_current_plugin()).busy,
            ));
            return false;
        }
        // The timer of the previous unit still runs. A queued unit re-arms it, so disarm it only before a wait.
        let wait = match mode {
            RecvMode::Try | RecvMode::Wait(0) => Wait::Never,
            RecvMode::Wait(-1) => Wait::Forever,
            RecvMode::Wait(t) => Wait::For(Duration::from_micros(t as u64)),
        };
        let mut disarmed = false;
        let got = pull_with(|units| {
            if !matches!(wait, Wait::Never) {
                let got = units.next(Wait::Never);
                if !matches!(got, Pull::NoUnit) {
                    return got;
                }
            }
            rapira_receive_untimed();
            disarmed = true;
            units.next(wait)
        });
        match got {
            Pull::Unit(unit) => {
                rapira_receive_timed();
                let ptr = NonNull::from(Box::leak(unit.into_held()));
                UNIT.set(Some(ptr.as_ptr()));
                // A Zend OOM bailout in the allocation longjmps through this frame, so it holds no value with Drop glue; the cycle slot then reclaims the unit.
                rapira_unit_object(rv, ptr.as_ref().class())
                    .cast::<HeldPtr>()
                    .write(HeldPtr(Some(ptr)));
                true
            }
            Pull::Closed | Pull::Reload => {
                if !disarmed {
                    rapira_receive_untimed();
                }
                zend::throw_exception(rapira_ce_closed_exception, CLOSED);
                false
            }
            Pull::NoUnit if matches!(mode, RecvMode::Try) => false,
            Pull::NoUnit => {
                zend::throw_exception(
                    rapira_ce_timeout_exception,
                    c"no work became available within the timeout",
                );
                false
            }
        }
    }
}

/// Returns true when `rv` holds the object of a pulled unit. Returns false with an exception set for these cases: closed, busy or timeout. For tryReceive, false with no exception means that no unit is queued, and the shell returns null.
/// # Safety
/// `timeout_us >= -1`. `rv` is the return value of the receive shell. The receive functions touch the zend timer, so the engine must be active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_receive(timeout_us: i64, rv: *mut zval) -> bool {
    guard(false, || unsafe {
        receive_into(rv, RecvMode::Wait(timeout_us))
    })
}

/// As `rapira_rs_receive`, but it never blocks.
/// # Safety
/// As `rapira_rs_receive`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_try_receive(rv: *mut zval) -> bool {
    guard(false, || unsafe { receive_into(rv, RecvMode::Try) })
}

/// Writes the two counts of a DispatcherInfo object: the intake depth, and whether the unit handed out last is unfinalized.
/// # Safety
/// `pending` and `active` are writable, and the engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_dispatcher_counts(pending: *mut i64, active: *mut i64) {
    guard((), || unsafe {
        *pending = pending_depth() as i64;
        *active = i64::from(UNIT.get().is_some_and(|p| !(*p).is_finalized()));
    })
}
