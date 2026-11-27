//! This module implements writeHead(), writeBody(), writeTrailers() and flush() over the HTTP responder.

use rapira_sapi::zend_string;

use super::headers::{forbidden_trailer, split_framing, walk_head_table};
use super::*;

/// A core returns a Verb and throws nothing. `throw_verb` throws after the core dropped its owned state, because `zend_throw_*` can bail out.
#[derive(Debug, PartialEq)]
pub(super) enum Verb {
    Ok,
    Finalized,
    HeadWritten,
    Discarded,
    ContentLengthExceeded,
    BadField(&'static CStr),
    FileNotSendable(&'static CStr),
    HeadNotWritten,
}

/// Throws the exception of `v`. Returns true for `Verb::Ok`, which throws nothing.
/// # Safety
/// The engine is active. The call can bail out on OOM.
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

/// Emits the committed or implicit 200 response head onto the wire.
pub(super) fn emit_head(st: &mut ExchangeState, finalizing_len: Option<u64>) -> Result<(), Closed> {
    let head = match std::mem::replace(&mut st.phase, Phase::Sent) {
        Phase::Open => ResponseHead::new(200),
        Phase::Committed(head) => head,
        phase => {
            st.phase = phase;
            return Ok(());
        }
    };
    let content_length = if st.bodiless {
        st.declared_cl
    } else {
        st.declared_cl.or(finalizing_len)
    };
    let Some(responder) = st.responder.as_mut() else {
        return Err(Closed);
    };
    responder
        .write_head(head, content_length, st.bodiless)
        .map_err(|_| Closed)
}

/// Sets `Phase::Discarded` so `release()` does not count the unit a second time.
pub(super) fn discard_unit(st: &mut ExchangeState) {
    if st.settled() {
        return;
    }
    st.phase = Phase::Discarded;
    st.unlink_spools();
    record(Event::Handled { errored: true });
    if let Some(mut responder) = st.responder.take() {
        responder.discard();
    }
}

/// The check before a write. It discards a cancelled unit. A settled unit takes no more writes.
pub(super) fn closed(st: &mut ExchangeState) -> Option<Verb> {
    if st.cancelled() {
        discard_unit(st);
        return Some(Verb::Discarded);
    }
    st.settled().then_some(Verb::Finalized)
}

/// # Safety
/// The engine is active on this thread.
pub(super) unsafe fn write_trailers_core(st: &mut ExchangeState) -> Verb {
    if let Some(verb) = closed(st) {
        return verb;
    }
    if matches!(st.phase, Phase::Open) {
        return Verb::HeadNotWritten;
    }
    if matches!(st.phase, Phase::Committed(_)) && st.sent_body == 0 {
        return send_complete(st, &[], 0);
    }
    if emit_head(st, Some(st.sent_body)).is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    unsafe { seal(st) };
    Verb::Ok
}

/// # Safety
/// `held` is the slot of a live exchange. `trailers` is a live array that ZPP owns. The engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_trailers(
    held: *const HeldPtr,
    trailers: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        let headers = match walk_head_table(trailers) {
            Ok(headers) => headers,
            Err(msg) => {
                zend::throw_value_error(msg);
                return false;
            }
        };
        let forbidden = headers
            .iter()
            .any(|(name, _)| forbidden_trailer(name.as_str()));
        // The plugin serves HTTP/1.1 and sends no trailer section, so only the checks read the headers.
        drop(headers);
        if forbidden {
            zend::throw_value_error(
                c"the field may not travel in a trailer section: framing, routing, authentication, request modifiers, response controls and content format stay in the head",
            );
            return false;
        }
        let st = &mut *(*held).state::<ExchangeState>();
        throw_verb(write_trailers_core(st))
    })
}

pub(super) fn write_head_core(
    st: &mut ExchangeState,
    status: u16,
    headers: ResponseHeaders,
) -> Verb {
    if st.cancelled() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if !matches!(st.phase, Phase::Open) {
        return Verb::HeadWritten;
    }
    // The contract calls an interim head advisory. HTTP/1.1 from PHP carries no interim head, so the plugin drops it here.
    if status != 101 && (100..200).contains(&status) {
        return Verb::Ok;
    }
    let split = match split_framing(headers) {
        Ok(split) => split,
        Err(msg) => return Verb::BadField(msg),
    };
    st.declared_cl = split.declared_cl;
    st.phase = Phase::Committed(ResponseHead {
        status,
        headers: split.headers,
        known_len: split.declared_cl,
    });
    // A 1xx response carries no body (RFC 9112 §6.3), the same as 204 and 304. A committed 101 therefore drops its chunks. https://www.rfc-editor.org/rfc/rfc9112#section-6.3
    if crate::responder::is_bodiless_status(status) {
        st.bodiless = true;
    }
    Verb::Ok
}

/// # Safety
/// `held` is the slot of a live exchange. `headers` is NULL or a live array that ZPP owns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_head(
    held: *const HeldPtr,
    status: i64,
    headers: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        let st = &mut *(*held).state::<ExchangeState>();
        if !(100..=599).contains(&status) {
            rapira_sapi::zend_value_error(
                c"status must be between 100 and 599, %lld given".as_ptr(),
                status as std::ffi::c_longlong,
            );
            return false;
        }
        let headers = match walk_head_table(headers) {
            Ok(headers) => headers,
            Err(msg) => {
                zend::throw_value_error(msg);
                return false;
            }
        };
        throw_verb(write_head_core(st, status as u16, headers))
    })
}

/// Sends a one-shot response (head + optional body) without allocating a stream channel and seals the exchange.
fn send_complete(st: &mut ExchangeState, body: &[u8], accepted_len: u64) -> Verb {
    st.unlink_spools();
    let head = match std::mem::replace(&mut st.phase, Phase::Open) {
        Phase::Open => ResponseHead::new(200),
        Phase::Committed(head) => head,
        other => {
            st.phase = other;
            unreachable!("send_complete requires Open or Committed phase");
        }
    };
    let content_length = if st.bodiless {
        st.declared_cl
    } else {
        st.declared_cl.or(Some(accepted_len))
    };
    let Some(mut responder) = st.responder.take() else {
        discard_unit(st);
        return Verb::Discarded;
    };
    let wire_body = if st.bodiless { &[] } else { body };
    if responder
        .write_complete(head, wire_body, content_length, st.bodiless, false)
        .is_err()
    {
        st.phase = Phase::Discarded;
        record(Event::Handled { errored: true });
        return Verb::Discarded;
    }
    st.sent_body = accepted_len;
    st.phase = Phase::Sealed;
    record(Event::Handled { errored: false });
    Verb::Ok
}

/// # Safety
/// `st` is valid. `s` is a live string. The engine is active.
pub(super) unsafe fn write_body_core(
    st: &mut ExchangeState,
    s: *mut zend_string,
    eos: bool,
) -> Verb {
    if let Some(verb) = closed(st) {
        return verb;
    }
    let bytes = unsafe { zend::zstr_bytes(s) };
    let len = bytes.len();
    if len == 0 && !eos {
        return Verb::Ok;
    }
    // One-shot fast path: hand head + body over the single oneshot without allocating a stream channel.
    if eos
        && st.sent_body == 0
        && matches!(st.phase, Phase::Open | Phase::Committed(_))
        && st.declared_cl.is_none_or(|cl| (len as u64) <= cl)
    {
        return send_complete(st, bytes, len as u64);
    }

    if let Some(cl) = st.declared_cl
        && st.sent_body + (len as u64) > cl
    {
        let fit = (cl - st.sent_body) as usize;
        if st.sent_body == 0 && matches!(st.phase, Phase::Open | Phase::Committed(_)) {
            let _ = send_complete(st, &bytes[..fit], cl);
            return Verb::ContentLengthExceeded;
        }
        if emit_head(st, Some(cl)).is_ok()
            && fit > 0
            && !st.bodiless
            && let Some(responder) = st.responder.as_mut()
        {
            let _ = unsafe { responder.write_chunk(&bytes[..fit]) };
        }
        st.sent_body = cl;
        unsafe { seal(st) };
        return Verb::ContentLengthExceeded;
    }

    if emit_head(st, None).is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    st.sent_body += len as u64;
    if len > 0 && !st.bodiless {
        let Some(responder) = st.responder.as_mut() else {
            discard_unit(st);
            return Verb::Discarded;
        };
        if unsafe { responder.write_chunk(bytes) }.is_err() {
            discard_unit(st);
            return Verb::Discarded;
        }
    }
    if eos {
        unsafe { seal(st) };
    }
    Verb::Ok
}

/// # Safety
/// `held` is the slot of a live exchange. `s` is a live string that ZPP holds for the call. The engine is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_body(
    held: *const HeldPtr,
    s: *mut zend_string,
    eos: bool,
) -> bool {
    guard(false, || unsafe {
        let st = &mut *(*held).state::<ExchangeState>();
        throw_verb(write_body_core(st, s, eos))
    })
}

/// # Safety
/// The engine is active on this thread.
pub(super) unsafe fn seal(st: &mut ExchangeState) {
    st.unlink_spools();
    st.phase = Phase::Sealed;
    record(Event::Handled { errored: false });
    if let Some(mut responder) = st.responder.take() {
        unsafe { responder.finish(false) };
    }
}

pub(super) fn flush_core(st: &mut ExchangeState) -> Verb {
    if let Some(verb) = closed(st) {
        return verb;
    }
    match emit_head(st, None) {
        Ok(()) => Verb::Ok,
        Err(Closed) => {
            discard_unit(st);
            Verb::Discarded
        }
    }
}

/// # Safety
/// `held` is the slot of a live exchange. The engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_flush(held: *const HeldPtr) -> bool {
    guard(false, || unsafe {
        let st = &mut *(*held).state::<ExchangeState>();
        throw_verb(flush_core(st))
    })
}

/// The quiesce hook of the http plugin record in rapira_http_classes.c.
/// Response bytes are copied or pooled before handoff, so quiesce is an immediate no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_http_quiesce() {}
