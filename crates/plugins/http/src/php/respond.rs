//! writeHead(), writeBody(), writeTrailers(), flush(), the frames they send and the strings that the IO thread reads in place.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

use rapira_sapi::{rapira_zstr_addref, rapira_zstr_release, zend_string};

use super::headers::{forbidden_trailer, split_framing, walk_head_table};
use super::*;

/// A core returns a Verb and throws nothing: `throw_verb` throws after the core dropped its owned state, because `zend_throw_*` can bail out.
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

/// The timer of zend_set_timeout on NTS: ITIMER_REAL on Apple Silicon, ITIMER_PROF (process CPU time) elsewhere. https://github.com/php/php-src/blob/php-8.5.12/Zend/zend_execute_API.c#L1647-L1655
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ZEND_TIMER: libc::c_int = libc::ITIMER_REAL;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const ZEND_TIMER: libc::c_int = libc::ITIMER_PROF;

/// On a full channel, park with the execution timer paused: a parked thread never reaches an opcode boundary, and on NTS a second expiry would `_exit(124)`. The resume restores the time that the timer had left.
/// # Safety
/// Engine active on this thread.
pub(super) unsafe fn send_frame(st: &mut ExchangeState, frame: Frame) -> Result<(), Closed> {
    let Some(tx) = st.sender.as_ref() else {
        return Err(Closed);
    };
    let sent = match tx.try_send(frame) {
        Ok(()) => Ok(()),
        Err(TrySendError::Closed(_)) => Err(Closed),
        Err(TrySendError::Full(frame)) => unsafe {
            let paused: libc::itimerval = std::mem::zeroed();
            let mut left: libc::itimerval = std::mem::zeroed();
            // SAFETY: both pointers are live itimerval values. https://man7.org/linux/man-pages/man2/setitimer.2.html
            libc::setitimer(ZEND_TIMER, &paused, &mut left);
            // Only Closed ends the park: a slow consumer is backpressure, not cancellation.
            let r = tx.blocking_send(frame).map_err(|_| Closed);
            // A zero value is a timer that was off or that expired: an expired timer already set EG(timed_out).
            if left.it_value.tv_sec != 0 || left.it_value.tv_usec != 0 {
                libc::setitimer(ZEND_TIMER, &left, std::ptr::null_mut());
            }
            r
        },
    };
    // A frame pushed while the receiver drops stays in the channel until the last Sender drops (tokio Rx::drop drains once). The drop of the Sender here frees it, so a lent string cannot hold the cycle end.
    if sent.is_ok() && tx.is_closed() {
        st.sender = None;
        return Err(Closed);
    }
    sent
}

/// `finalizing_len` is the one-shot body length used only when nothing streamed before: a declared content-length wins, and a bodiless response gets none.
/// # Safety
/// As `send_frame`.
pub(super) unsafe fn emit_head(
    st: &mut ExchangeState,
    finalizing_len: Option<u64>,
) -> Result<(), Closed> {
    let head = match std::mem::replace(&mut st.phase, Phase::Sent) {
        Phase::Open => ResponseHead {
            status: 200,
            headers: HeaderMap::new(),
        },
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
    unsafe {
        send_frame(
            st,
            Frame::Head {
                head,
                content_length,
                bodiless: st.bodiless,
            },
        )
    }
}

/// Setting `Phase::Discarded` here prevents a second count of the unit in `release()`. The discard also releases the strings that the IO thread no longer reads.
pub(super) fn discard_unit(st: &mut ExchangeState) {
    // SAFETY: every caller runs on the PHP thread with the engine active.
    unsafe { release_done() };
    if st.settled() {
        return;
    }
    st.phase = Phase::Discarded;
    st.body.unlink_spools();
    sb_update(Event::Handled { errored: true });
    // Every caller saw the channel closed first, so no frame can reach the plugin.
    st.sender = None;
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn write_trailers_core(st: &mut ExchangeState, trailers: HeaderMap) -> Verb {
    if st.cancelled() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.settled() {
        return Verb::Finalized;
    }
    if matches!(st.phase, Phase::Open) {
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
    unsafe { seal(st, trailers) };
    Verb::Ok
}

/// # Safety
/// `held` the slot of a live exchange; `trailers` a live, ZPP-owned array; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_trailers(
    held: *const HeldPtr,
    trailers: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        let map = match walk_head_table(trailers) {
            Ok(map) => map,
            Err(msg) => {
                zend::throw_value_error(msg);
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
        let st = &mut *(*held).state::<ExchangeState>();
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
    if st.cancelled() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if !matches!(st.phase, Phase::Open) {
        return Verb::HeadWritten;
    }
    // The contract calls an interim head advisory, and HTTP/1.1 from PHP carries none: the plugin drops it here.
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
    });
    // 1xx carries no body either (RFC 9112 §6.3), so a committed 101 drops chunks like 204/304. https://www.rfc-editor.org/rfc/rfc9112#section-6.3
    if matches!(status, 204 | 304 | 101) {
        st.bodiless = true;
    }
    Verb::Ok
}

/// # Safety
/// `held` the slot of a live exchange; `headers` NULL or a live, ZPP-owned array.
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

/// The smallest writeBody piece that the IO thread reads in place; a smaller piece is copied. A measurement sets the value.
const ZERO_COPY_MIN: usize = 16 << 10;

/// The most bytes of PHP strings that the IO thread holds in place at once; above it a piece is copied. It bounds what memory_limit sees for borrowed strings and how long the cycle end waits for slow clients: at most the cap at the client's read speed.
const BORROW_CAP: usize = 8 << 20;

/// # Safety
/// `st` valid; `s` a live string; engine active.
pub(super) unsafe fn write_body_core(
    st: &mut ExchangeState,
    s: *mut zend_string,
    eos: bool,
) -> Verb {
    unsafe { release_done() };
    if st.cancelled() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.settled() {
        return Verb::Finalized;
    }
    let len = unsafe { (*s).len };
    if len == 0 && !eos {
        return Verb::Ok;
    }
    unsafe {
        send_body(st, len as u64, eos, |n| {
            let n = n as usize;
            // A cut piece lends the whole string: the slice keeps all of it alive.
            if len >= ZERO_COPY_MIN && lending() && lent_bytes() + len <= BORROW_CAP {
                Frame::Chunk(borrow(s, len).slice(..n))
            } else {
                Frame::Chunk(Bytes::copy_from_slice(&zend::zstr_bytes(s)[..n]))
            }
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
        unsafe { seal(st, HeaderMap::new()) };
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
        unsafe { seal(st, HeaderMap::new()) };
    }
    Verb::Ok
}

/// # Safety
/// `held` the slot of a live exchange; `s` a live string that ZPP holds for the call; engine active.
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
/// As `send_frame`.
pub(super) unsafe fn seal(st: &mut ExchangeState, trailers: HeaderMap) {
    unsafe { release_done() };
    st.body.unlink_spools();
    st.phase = Phase::Sealed;
    sb_update(Event::Handled { errored: false });
    let _ = unsafe {
        send_frame(
            st,
            Frame::End {
                trailers,
                truncated: false,
            },
        )
    };
    st.sender = None;
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn flush_core(st: &mut ExchangeState) -> Verb {
    if st.cancelled() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.settled() {
        return Verb::Finalized;
    }
    match unsafe { emit_head(st, None) } {
        Ok(()) => Verb::Ok,
        Err(Closed) => {
            discard_unit(st);
            Verb::Discarded
        }
    }
}

/// # Safety
/// `held` the slot of a live exchange; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_flush(held: *const HeldPtr) -> bool {
    guard(false, || unsafe {
        let st = &mut *(*held).state::<ExchangeState>();
        throw_verb(flush_core(st))
    })
}

/// A string that the IO thread reads in place: the Bytes owner, and the LENT entry of the PHP thread. A drop on any thread only lowers the count of `alive`.
#[derive(Clone)]
struct Borrowed {
    s: *mut zend_string,
    len: usize,
    alive: Arc<()>,
}

// SAFETY: the IO thread only reads the bytes; the PHP thread changes the reference count.
unsafe impl Send for Borrowed {}

impl AsRef<[u8]> for Borrowed {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: borrow() took a reference, and release_done() releases it only after the last Bytes owner dropped.
        unsafe { zend::zstr_bytes(self.s) }
    }
}

thread_local! {
    /// The strings of this PHP thread that the IO thread can still read.
    static LENT: RefCell<Vec<Borrowed>> = const { RefCell::new(Vec::new()) };
    /// The cycle whose quiesce ran: user code that a later RSHUTDOWN runs (a session save handler) copies, because the memory manager frees the request strings before the next quiesce.
    static QUIESCED_CYCLE: Cell<u64> = const { Cell::new(0) };
}

/// A piece can be lent: the cycle is not past its quiesce.
fn lending() -> bool {
    rapira_sapi::exchange::cycle() != QUIESCED_CYCLE.get()
}

fn lent_bytes() -> usize {
    LENT.with_borrow(|lent| lent.iter().map(|b| b.len).sum())
}

/// A frame body that reads `s` in place; the new reference keeps `s` alive until release_done().
/// # Safety
/// `s` a live string of `len` bytes; engine active on this thread.
unsafe fn borrow(s: *mut zend_string, len: usize) -> Bytes {
    unsafe { rapira_zstr_addref(s) };
    let alive = Arc::new(());
    let b = Borrowed { s, len, alive };
    LENT.with_borrow_mut(|lent| lent.push(b.clone()));
    Bytes::from_owner(b)
}

/// Releases the strings that the IO thread no longer reads. write_body_core, seal and discard_unit call it first, so a long response releases its strings while it runs. Arc::get_mut loads the count with Acquire, so the reads of the IO thread come before the release.
/// # Safety
/// Engine active on this thread.
unsafe fn release_done() {
    LENT.with_borrow_mut(|lent| {
        for b in lent.extract_if(.., |b| Arc::get_mut(&mut b.alive).is_some()) {
            unsafe { rapira_zstr_release(b.s) };
        }
    });
}

/// The quiesce hook of the http plugin record (rapira_http_classes.c): waits until the IO thread reads no lent string, then releases them. A warn record about every 5 s names what the IO thread still holds.
/// # Safety
/// Engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_http_quiesce() {
    guard((), || unsafe {
        for polls in 0u64.. {
            release_done();
            let held = LENT.with_borrow(Vec::len);
            if held == 0 {
                QUIESCED_CYCLE.set(rapira_sapi::exchange::cycle());
                return;
            }
            if polls == 0 {
                // the request watchdog does not count the wait as a stuck request
                sb_update(Event::Idle);
            }
            if polls % 5000 == 4999 {
                tracing::warn!(
                    target: "http",
                    "the cycle end waits for {held} response piece(s) of {} bytes that the IO thread still holds",
                    lent_bytes()
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
}
