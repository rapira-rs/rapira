use super::headers::{forbidden_trailer, split_framing, walk_head_table};
use super::*;

/// Cores return these instead of throwing: no owned state may be live when `zend_throw_*` bailouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verb {
    Ok,
    Finalized,
    HeadWritten,
    Overflow,
    Discarded,
    ContentLengthExceeded,
    BadField(&'static CStr),
    FileNotSendable(&'static CStr),
    HeadNotWritten,
}

/// Throws the exception of `v`. True for `Verb::Ok`, which throws nothing.
/// # Safety
/// Engine active; can bailout on OOM.
pub(super) unsafe fn throw_verb(v: Verb) -> bool {
    unsafe {
        match v {
            Verb::Ok => return true,
            Verb::Finalized => zend::throw_exception(
                rapira_ce_already_finalized_error,
                c"the response already ended",
            ),
            Verb::HeadWritten => zend::throw_exception(
                rapira_ce_http_head_already_written_error,
                c"the final head has already been written",
            ),
            Verb::Overflow => zend::throw_error(c"response chunk exceeds the host buffer cap"),
            Verb::Discarded => zend::throw_exception(
                rapira_ce_work_discarded_exception,
                c"the host closed the exchange first",
            ),
            Verb::ContentLengthExceeded => zend::throw_exception(
                rapira_ce_http_content_length_exceeded_error,
                c"the write goes past the content-length the head declared",
            ),
            Verb::BadField(msg) => zend::throw_value_error(msg),
            Verb::HeadNotWritten => zend::throw_exception(
                rapira_ce_http_head_not_written_error,
                c"no final head has been committed yet",
            ),
            Verb::FileNotSendable(msg) => {
                zend::throw_exception(rapira_ce_http_file_not_sendable_exception, msg);
            }
        }
    }
    false
}

pub(super) struct Closed;

/// On a full channel, park with the wall timer disarmed: a parked thread never reaches an opcode boundary, and on NTS a second expiry would `_exit(124)`; the re-arm grants the remaining budget (floor 1s).
/// # Safety
/// Engine active on this thread.
pub(super) unsafe fn send_frame(st: &mut ExchangeState, frame: Frame) -> Result<(), Closed> {
    let (result, parked) = {
        let Some(tx) = st.ctx.sender.as_ref() else {
            return Err(Closed);
        };
        match tx.try_send(frame) {
            Ok(()) => (Ok(()), false),
            Err(TrySendError::Closed(_)) => (Err(Closed), false),
            Err(TrySendError::Full(frame)) => unsafe {
                let consumed = st.armed_at.elapsed();
                let saved = (*rapira_eg()).timeout_seconds;
                if saved > 0 {
                    zend_unset_timeout();
                }
                // Only `Closed` ends the park: a slow consumer is backpressure, not cancellation.
                let r = tx.blocking_send(frame).map_err(|_| Closed);
                if saved > 0 {
                    let remaining = (saved as u64).saturating_sub(consumed.as_secs()).max(1);
                    zend_set_timeout(remaining as rapira_sapi::zend_long, false);
                }
                (r, saved > 0)
            },
        }
    };
    if parked {
        st.armed_at = Instant::now();
    }
    result
}

/// `finalizing_len` is the one-shot body length used only when nothing streamed before: a declared content-length wins, and a bodiless response gets none.
/// # Safety
/// As `send_frame`.
pub(super) unsafe fn emit_head(
    st: &mut ExchangeState,
    finalizing_len: Option<u64>,
) -> Result<(), Closed> {
    if st.head_sent {
        return Ok(());
    }
    let (status, headers) = match st.pending.take() {
        Some(p) => (p.status, p.headers),
        None => (200, HeaderMap::new()),
    };
    if st.stage == Stage::Open {
        st.stage = Stage::HeadCommitted;
    }
    let content_length = if st.bodiless {
        st.declared_cl
    } else {
        st.declared_cl.or(finalizing_len)
    };
    st.head_sent = true;
    unsafe {
        send_frame(
            st,
            Frame::Head {
                head: ResponseHead { status, headers },
                content_length,
                bodiless: st.bodiless,
            },
        )
    }
}

/// Setting `Stage::Finalized` here keeps `exchange_drop`/`reclaim_current` from counting the unit a second time.
pub(super) fn discard_unit(st: &mut ExchangeState) {
    if st.stage == Stage::Finalized {
        return;
    }
    st.discarded = true;
    st.stage = Stage::Finalized;
    st.body.unlink_spools();
    sb_update(Event::Handled(true));
    if let Some(tx) = st.ctx.sender.take() {
        let _ = tx.try_send(Frame::End {
            trailers: HeaderMap::new(),
            truncated: true,
        });
    }
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn write_trailers_core(st: &mut ExchangeState, trailers: HeaderMap) -> Verb {
    if st.client_closed() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.stage == Stage::Finalized {
        return Verb::Finalized;
    }
    if st.stage == Stage::Open {
        return Verb::HeadNotWritten;
    }
    if unsafe { emit_head(st, Some(st.sent_body)) }.is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    let trailers = if st.bodiless {
        HeaderMap::new()
    } else {
        trailers
    };
    unsafe {
        seal(st, /*truncated=*/ false, trailers)
    };
    Verb::Ok
}

/// # Safety
/// `job` from receive; `trailers` a live, ZPP-owned array; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_trailers(
    job: *mut c_void,
    trailers: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        let map = match walk_head_table(trailers) {
            Ok(map) => map,
            Err(_) => {
                zend::throw_value_error(
                    c"a trailer name or value is not representable on the wire",
                );
                return false;
            }
        };
        if map.keys().any(|n| forbidden_trailer(n.as_str())) {
            drop(map);
            zend::throw_value_error(
                c"the field may not travel in a trailer section: framing, routing, authentication, request modifiers, response controls and content format stay in the head",
            );
            return false;
        }
        let st = &mut *job.cast::<ExchangeState>();
        throw_verb(write_trailers_core(st, map))
    })
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn write_head_core(
    st: &mut ExchangeState,
    status: u16,
    headers: HeaderMap,
) -> Verb {
    if st.client_closed() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.stage != Stage::Open {
        return Verb::HeadWritten;
    }
    if status != 101 && (100..200).contains(&status) {
        let head = ResponseHead { status, headers };
        return match unsafe { send_frame(st, Frame::Interim(head)) } {
            Ok(()) => Verb::Ok,
            Err(Closed) => {
                discard_unit(st);
                Verb::Discarded
            }
        };
    }
    let split = match split_framing(headers) {
        Ok(split) => split,
        Err(msg) => return Verb::BadField(msg),
    };
    st.declared_cl = split.declared_cl;
    st.pending = Some(PendingHead {
        status,
        headers: split.headers,
    });
    // 1xx carries no body either (RFC 9112 §6.3), so a committed 101 drops chunks like 204/304.
    if matches!(status, 204 | 304 | 101) {
        st.bodiless = true;
    }
    st.stage = Stage::HeadCommitted;
    Verb::Ok
}

/// # Safety
/// `job` from receive; `headers` NULL or a live, ZPP-owned array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_head(
    job: *mut c_void,
    status: i64,
    headers: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        let st = &mut *job.cast::<ExchangeState>();
        if !(100..=599).contains(&status) {
            rapira_sapi::zend_value_error(
                c"status must be between 100 and 599, %lld given".as_ptr(),
                status as std::ffi::c_longlong,
            );
            return false;
        }
        let map = match walk_head_table(headers) {
            Ok(map) => map,
            Err(msg) => {
                zend::throw_value_error(msg);
                return false;
            }
        };
        throw_verb(write_head_core(st, status as u16, map))
    })
}

/// # Safety
/// `st` valid; `p` points at `len` readable bytes; engine active.
pub(super) unsafe fn write_body_core(
    st: &mut ExchangeState,
    p: *const c_char,
    len: usize,
    eos: bool,
) -> Verb {
    if st.client_closed() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.stage == Stage::Finalized {
        return Verb::Finalized;
    }
    if len == 0 && !eos {
        return Verb::Ok;
    }
    if len > MAX_BUFFERED_BODY {
        tracing::error!(
            target: "rapira",
            "response chunk exceeds the host buffer cap ({len} > {MAX_BUFFERED_BODY} bytes); sealing truncated"
        );
        let _ = unsafe { emit_head(st, None) };
        unsafe {
            seal(st, /*truncated=*/ true, HeaderMap::new())
        };
        return Verb::Overflow;
    }
    unsafe {
        send_body(st, len as u64, eos, |n| {
            Frame::Chunk(Bytes::copy_from_slice(std::slice::from_raw_parts(
                p.cast::<u8>(),
                n as usize,
            )))
        })
    }
}

/// Sends `len` body bytes and seals on `eos`. `frame(n)` builds the frame of the first `n` bytes; a write past the declared content-length sends the part that fits and seals.
/// # Safety
/// As `send_frame`.
pub(super) unsafe fn send_body(
    st: &mut ExchangeState,
    len: u64,
    eos: bool,
    frame: impl FnOnce(u64) -> Frame,
) -> Verb {
    if let Some(cl) = st.declared_cl
        && st.sent_body + len > cl
    {
        let fit = cl - st.sent_body;
        if unsafe { emit_head(st, Some(cl)) }.is_ok() && fit > 0 && !st.bodiless {
            let _ = unsafe { send_frame(st, frame(fit)) };
        }
        st.sent_body = cl;
        unsafe {
            seal(st, /*truncated=*/ false, HeaderMap::new())
        };
        return Verb::ContentLengthExceeded;
    }
    let finalizing = (eos && st.sent_body == 0).then_some(len);
    if unsafe { emit_head(st, finalizing) }.is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    st.sent_body += len;
    if len > 0 && !st.bodiless && unsafe { send_frame(st, frame(len)) }.is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if eos {
        unsafe {
            seal(st, /*truncated=*/ false, HeaderMap::new())
        };
    }
    Verb::Ok
}

/// # Safety
/// `job` from receive; `p` points at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_body(
    job: *mut c_void,
    p: *const c_char,
    len: usize,
    eos: bool,
) -> bool {
    guard(false, || unsafe {
        let st = &mut *job.cast::<ExchangeState>();
        throw_verb(write_body_core(st, p, len, eos))
    })
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn seal(st: &mut ExchangeState, truncated: bool, trailers: HeaderMap) {
    st.body.unlink_spools();
    st.stage = Stage::Finalized;
    sb_update(Event::Handled(truncated));
    let _ = unsafe {
        send_frame(
            st,
            Frame::End {
                trailers,
                truncated,
            },
        )
    };
    st.ctx.sender = None;
}

/// # Safety
/// `job` from receive; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_flush(job: *mut c_void) -> bool {
    guard(false, || unsafe {
        let st = &mut *job.cast::<ExchangeState>();
        let v = if st.client_closed() {
            discard_unit(st);
            Verb::Discarded
        } else if st.stage == Stage::Finalized {
            Verb::Finalized
        } else {
            match emit_head(st, None) {
                Ok(()) => Verb::Ok,
                Err(Closed) => {
                    discard_unit(st);
                    Verb::Discarded
                }
            }
        };
        throw_verb(v)
    })
}

/// # Safety
/// `job` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_is_finalized(job: *const c_void) -> bool {
    guard(false, || unsafe {
        (*job.cast::<ExchangeState>()).is_finalized()
    })
}

/// # Safety
/// `job` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_is_cancelled(job: *const c_void) -> bool {
    guard(false, || unsafe {
        let st = &*job.cast::<ExchangeState>();
        st.client_closed()
    })
}

/// Reclaims the Box on free_obj; a unit lost to a bailout (fatal, timeout) skips the failure frames, so the plugin reports the worker death when the sender drops.
/// # Safety
/// `job` is a non-null pointer produced by `Box::into_raw` in receive; free_obj checks for NULL before the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_drop(job: *mut c_void) {
    guard((), || {
        let mut st = unsafe { release(job.cast::<ExchangeState>()) };
        let cycle_died = unsafe { (*rapira_sapi::rapira_cg()).unclean_shutdown };
        if st.stage != Stage::Finalized && !cycle_died {
            st.body.unlink_spools();
            if let Some(tx) = st.ctx.sender.take() {
                if st.head_sent {
                    let _ = tx.try_send(Frame::End {
                        trailers: HeaderMap::new(),
                        truncated: true,
                    });
                } else if tx
                    .try_send(Frame::Head {
                        head: ResponseHead {
                            status: 500,
                            headers: HeaderMap::new(),
                        },
                        content_length: (!st.bodiless).then_some(0),
                        bodiless: st.bodiless,
                    })
                    .is_ok()
                {
                    let _ = tx.try_send(Frame::End {
                        trailers: HeaderMap::new(),
                        truncated: false,
                    });
                }
            }
        }
        drop(st);
    })
}
