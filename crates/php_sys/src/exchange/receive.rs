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
        if let Unit::Handling(ptr) = CURRENT.get()
            && (*ptr).host_closed()
        {
            tracing::debug!(target: "rapira", "receive() discarded an unfinalized exchange whose client left");
            super::respond::discard_unit(&mut *ptr);
        }
        if matches!(CURRENT.get(), Unit::Handling(_)) {
            zend::throw_error(
                c"receive() while a Rapira\\Http\\Exchange is unfinalized; finalize it first",
            );
            return false;
        }
        let mut obj: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut obj, rapira_ce_internal_http_exchange);
        let wait = ReceiveWait::new(match mode {
            RecvMode::Try => 0,
            RecvMode::Wait(t) => t,
        });
        loop {
            // SAFETY: plain Zend timer bookkeeping on this thread; no bailout path.
            rapira_receive_untimed();
            let pulled = wait.pull();
            rapira_receive_timed();
            match pulled {
                Pulled::Job(job) => {
                    let Work::Http(job) = job else {
                        job.unavailable();
                        continue;
                    };
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
                    CURRENT.set(Unit::Handling(ptr));
                    work::note_received();
                    (*exchange_from(obj.value.obj)).job = ptr.cast();
                    (*ptr).armed_at = Instant::now();
                    *return_value = obj;
                    return true;
                }
                Pulled::Closed => {
                    work::note_closed();
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
        (*info).active = i64::from(matches!(CURRENT.get(), Unit::Handling(_)));
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
                let class = if crate::grpc::installed() {
                    crate::rapira_ce_internal_grpc_dispatcher
                } else {
                    rapira_ce_internal_http_dispatcher
                };
                let _ = object_init_ex(&mut zv, class);
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
