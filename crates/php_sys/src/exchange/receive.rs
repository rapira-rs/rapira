use super::*;

enum RecvMode {
    Wait(i64),
    Try,
}

/// Allocates the wrapper object before pulling: a unit pulled with no owner would leak its Frame sender and hang the client.
/// # Safety
/// `return_value` writable; engine active on this thread.
unsafe fn receive_into(return_value: *mut zval, mode: RecvMode) -> bool {
    unsafe {
        if let Unit::Handling(ptr) = CYCLE.get().unit
            && (*ptr).host_closed()
        {
            super::respond::discard_unit(&mut *ptr);
        }
        if matches!(CYCLE.get().unit, Unit::Handling(_)) {
            zend::throw_error(
                c"receive() while a Rapira\\Http\\Exchange is unfinalized; finalize it first",
            );
            return false;
        }
        let mut obj: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut obj, rapira_ce_internal_http_exchange);
        // SAFETY: plain zend timer bookkeeping on this thread; no bailout path.
        rapira_receive_untimed();
        loop {
            let pulled = match mode {
                RecvMode::Try | RecvMode::Wait(0) => pull_job_try(),
                RecvMode::Wait(-1) => pull_job_wait(None),
                RecvMode::Wait(t) => pull_job_wait(Some(Duration::from_micros(t as u64))),
            };
            match pulled {
                Pulled::Job(job) => {
                    let st = match ExchangeState::new(job) {
                        Ok(st) => st,
                        Err(mut job) => {
                            job.ctx.finish(true);
                            sb_update(Event::Handled(true));
                            continue;
                        }
                    };
                    if st.job.ctx.sender.as_ref().is_some_and(Sender::is_closed) {
                        sb_update(Event::Handled(true));
                        continue;
                    }
                    let ptr = Box::into_raw(Box::new(st));
                    update(|c| {
                        c.unit = Unit::Handling(ptr);
                        c.received = true;
                    });
                    (*exchange_from(obj.value.obj)).job = ptr.cast();
                    // SAFETY: plain zend timer bookkeeping; no bailout path.
                    rapira_receive_timed();
                    (*ptr).armed_at = Instant::now();
                    *return_value = obj;
                    return true;
                }
                Pulled::Closed => {
                    update(|c| c.closed_seen = true);
                    zval_ptr_dtor(&mut obj);
                    zend::throw_exception(
                        rapira_ce_closed_exception,
                        c"no more work will ever arrive",
                    );
                    return false;
                }
                Pulled::Empty if matches!(mode, RecvMode::Try) => {
                    zval_ptr_dtor(&mut obj);
                    zend::zval_null(return_value);
                    return true;
                }
                Pulled::Timeout | Pulled::Empty => {
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
        let _ = object_init_ex(return_value, rapira_ce_internal_http_dispatcher_info);
        let info = info_from((*return_value).value.obj);
        (*info).pending = pending_depth() as i64;
        (*info).active = i64::from(matches!(CYCLE.get().unit, Unit::Handling(_)));
        true
    })
}

thread_local! {
    static DISPATCHER: Cell<Option<zval>> = const { Cell::new(None) };
}

/// # Safety
/// `return_value` writable; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_get_dispatcher(return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        if crate::rapira_mode != RAPIRA_MODE_DISPATCHER as c_int {
            zend::throw_exception(
                rapira_ce_no_dispatcher_error,
                c"nothing dispatches work to this process outside dispatcher mode",
            );
            return false;
        }
        let inst = DISPATCHER.with(|d| match d.get() {
            Some(zv) => zv,
            None => {
                let mut zv: zval = std::mem::zeroed();
                let _ = object_init_ex(&mut zv, rapira_ce_internal_http_dispatcher);
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
