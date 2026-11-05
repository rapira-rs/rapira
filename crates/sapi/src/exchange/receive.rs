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
        let classes = classes();
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
            zend::throw_error(classes.busy);
            return false;
        }
        let mut obj: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut obj, (classes.unit)());
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
                    // SAFETY: obj is a live object of classes.unit, allocated above on this thread.
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
/// `return_value` writable; engine active on this thread (the receive verbs touch the zend timer).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_receive(timeout_us: i64, return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        if timeout_us < -1 {
            crate::zend_argument_value_error(1, c"must be greater than or equal to -1".as_ptr());
            return false;
        }
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

/// # Safety
/// `return_value` writable; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_dispatcher_info(return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        let _ = object_init_ex(return_value, (classes().info)());
        let info = info_from((*return_value).value.obj);
        (*info).pending = pending_depth() as i64;
        (*info).active = i64::from(UNIT.get().is_some_and(|p| !(*p).is_finalized()));
        true
    })
}

thread_local! {
    pub(super) static DISPATCHER: Cell<Option<zval>> = const { Cell::new(None) };
}

/// # Safety
/// `return_value` writable; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_get_dispatcher(return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        if crate::rapira_mode != RAPIRA_MODE_DISPATCHER as c_int {
            zend::throw_exception(
                rapira_ce_no_dispatcher_error,
                c"Dispatcher is only available in Mode::Dispatcher",
            );
            return false;
        }
        let inst = DISPATCHER.with(|d| match d.get() {
            Some(zv) => zv,
            None => {
                let mut zv: zval = std::mem::zeroed();
                let _ = object_init_ex(&mut zv, (classes().dispatcher)());
                d.set(Some(zv));
                zv
            }
        });
        *return_value = inst;
        zval_add_ref(return_value);
        true
    })
}

/// Called from the C RSHUTDOWN bracket.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_dispatcher_release() {
    guard((), || {
        DISPATCHER.with(|d| {
            if let Some(mut zv) = d.take() {
                // SAFETY: the zval came from object_init_ex on this thread.
                unsafe { zval_ptr_dtor(&mut zv) };
            }
        });
    })
}
