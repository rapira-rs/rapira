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

/// # Safety
/// Engine active; can bailout on OOM.
pub(super) unsafe fn throw_verb(v: Verb) {
    unsafe {
        match v {
            Verb::Ok => {}
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
}

pub(super) struct Closed;

/// On a full channel, park with the wall timer disarmed: a parked thread never reaches an opcode boundary, and on NTS a second expiry would `_exit(124)`; the re-arm grants the remaining budget (floor 1s).
/// # Safety
/// Engine active on this thread.
pub(super) unsafe fn send_frame(st: &mut ExchangeState, frame: Frame) -> Result<(), Closed> {
    let (result, parked) = {
        let Some(tx) = st.job.ctx.sender.as_ref() else {
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
                let r = park_send(tx, frame);
                if saved > 0 {
                    let remaining = (saved as u64).saturating_sub(consumed.as_secs()).max(1);
                    zend_set_timeout(remaining as crate::zend_long, false);
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

/// Only `Closed` ends the park: a slow consumer is backpressure, not cancellation.
pub(super) fn park_send(tx: &Sender<Frame>, mut frame: Frame) -> Result<(), Closed> {
    let mut spins = 0u32;
    loop {
        match tx.try_send(frame) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Closed(_)) => return Err(Closed),
            Err(TrySendError::Full(f)) => {
                frame = f;
                if spins < 64 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_micros(100));
                }
                spins = spins.saturating_add(1);
            }
        }
    }
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
    if let BodyState::Multipart { files, .. } = &mut st.body {
        for p in files {
            p.upload.file.unlink();
        }
    }
    sb_update(Event::Handled(true));
    if let Some(tx) = st.job.ctx.sender.take() {
        let _ = tx.try_send(Frame::End {
            trailers: HeaderMap::new(),
            truncated: true,
        });
    }
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn write_trailers_core(st: &mut ExchangeState, trailers: HeaderMap) -> Verb {
    if st.host_closed() {
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
        match write_trailers_core(st, map) {
            Verb::Ok => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn write_head_core(
    st: &mut ExchangeState,
    status: u16,
    headers: HeaderMap,
) -> Verb {
    if st.host_closed() {
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
            crate::zend_value_error(
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
        match write_head_core(st, status as u16, map) {
            Verb::Ok => true,
            v => {
                throw_verb(v);
                false
            }
        }
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
    if st.host_closed() {
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
    let len64 = len as u64;
    if let Some(cl) = st.declared_cl
        && st.sent_body + len64 > cl
    {
        let fit = usize::try_from(cl - st.sent_body).unwrap_or(usize::MAX);
        if unsafe { emit_head(st, Some(cl)) }.is_ok() && fit > 0 && !st.bodiless {
            let bytes =
                Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), fit) });
            let _ = unsafe { send_frame(st, Frame::Chunk(bytes)) };
        }
        st.sent_body = cl;
        unsafe {
            seal(st, /*truncated=*/ false, HeaderMap::new())
        };
        return Verb::ContentLengthExceeded;
    }
    let finalizing = (eos && st.sent_body == 0).then_some(len64);
    if unsafe { emit_head(st, finalizing) }.is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    st.sent_body += len64;
    if len > 0 && !st.bodiless {
        let bytes =
            Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) });
        if unsafe { send_frame(st, Frame::Chunk(bytes)) }.is_err() {
            discard_unit(st);
            return Verb::Discarded;
        }
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
        match write_body_core(st, p, len, eos) {
            Verb::Ok => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn seal(st: &mut ExchangeState, truncated: bool, trailers: HeaderMap) {
    if let BodyState::Multipart { files, .. } = &mut st.body {
        for p in files {
            p.upload.file.unlink();
        }
    }
    st.stage = Stage::Finalized;
    update(|c| c.served = true);
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
    st.job.ctx.sender = None;
}

/// # Safety
/// `job` from receive; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_flush(job: *mut c_void) -> bool {
    guard(false, || unsafe {
        let st = &mut *job.cast::<ExchangeState>();
        let v = if st.host_closed() {
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
        match v {
            Verb::Ok => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}

/// # Safety
/// `job` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_is_finalized(job: *const c_void) -> bool {
    guard(false, || unsafe {
        let st = &*job.cast::<ExchangeState>();
        st.stage == Stage::Finalized || st.job.ctx.sender.as_ref().is_some_and(Sender::is_closed)
    })
}

/// # Safety
/// `job` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_is_cancelled(job: *const c_void) -> bool {
    guard(false, || unsafe {
        let st = &*job.cast::<ExchangeState>();
        st.host_closed()
    })
}

/// Reclaims the Box on free_obj; a unit lost to a bailout (fatal, timeout) skips the failure frames, so the host's deadline reports the worker death instead.
/// # Safety
/// `job` is a non-null pointer produced by `Box::into_raw` in receive; free_obj checks for NULL before the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_drop(job: *mut c_void) {
    guard((), || {
        let ptr: *mut ExchangeState = job.cast();
        update(|c| {
            if c.unit == Some(ptr) {
                c.unit = None;
            }
        });
        let mut st = unsafe { Box::from_raw(ptr) };
        let cycle_died = unsafe { (*crate::rapira_cg()).unclean_shutdown };
        if st.stage != Stage::Finalized {
            sb_update(Event::Handled(true));
        }
        if st.stage != Stage::Finalized && !cycle_died {
            if let BodyState::Multipart { files, .. } = &mut st.body {
                for p in files {
                    p.upload.file.unlink();
                }
            }
            if let Some(tx) = st.job.ctx.sender.take() {
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
