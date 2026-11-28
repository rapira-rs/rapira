//! PHP-to-IO-shard response handoff: single-Arc lock-free oneshot slot, recycled large buffers, and `HttpResponder`.

use std::cell::UnsafeCell;
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use http::header::CONTENT_LENGTH;
use rapira_http_middleware::ResponseHead;
use rapira_sapi::CgiResponder;
use tokio::sync::mpsc;

const STREAM_CHANNEL_CAP: usize = 16;
const POOL_MIN_BYTES: usize = 32 << 10;
const POOL_MAX_BYTES: usize = 4 << 20;
const POOL_MAX_BUFS: usize = 32;

static BUF_POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Owned buffer returned to `BUF_POOL` when `hyper` drops the `Bytes` after `writev()`.
pub(super) struct PooledBuf {
    pub(super) buf: Vec<u8>,
}

impl AsRef<[u8]> for PooledBuf {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        if self.buf.capacity() >= POOL_MIN_BYTES
            && self.buf.capacity() <= POOL_MAX_BYTES
            && let Ok(mut pool) = BUF_POOL.lock()
            && pool.len() < POOL_MAX_BUFS
        {
            self.buf.clear();
            pool.push(std::mem::take(&mut self.buf));
        }
    }
}

/// Copies a PHP slice into `Bytes`, using pre-faulted pooled storage for large bodies to avoid `mmap` churn.
fn copy_response_bytes(src: &[u8]) -> Bytes {
    if src.is_empty() {
        return Bytes::new();
    }
    if src.len() < POOL_MIN_BYTES || src.len() > POOL_MAX_BYTES {
        return Bytes::copy_from_slice(src);
    }
    let mut buf = take_pooled_buf(src.len());
    buf.extend_from_slice(src);
    Bytes::from_owner(PooledBuf { buf })
}

pub(super) fn take_pooled_buf(min_cap: usize) -> Vec<u8> {
    let popped = if let Ok(mut pool) = BUF_POOL.lock() {
        if let Some(pos) = pool.iter().position(|b| b.capacity() >= min_cap) {
            return pool.swap_remove(pos);
        }
        pool.pop()
    } else {
        None
    };
    match popped {
        Some(mut b) => {
            b.reserve(min_cap);
            b
        }
        None => Vec::with_capacity(min_cap),
    }
}

const SLOT_EMPTY: u8 = 0;
const SLOT_REGISTERING: u8 = 1;
const SLOT_WAKER_SET: u8 = 2;
const SLOT_READY: u8 = 3;
const SLOT_CLOSED: u8 = 4;

/// Single-allocation lock-free oneshot + cancellation state shared between one IO shard request and one PHP worker.
pub(crate) struct ResponseSlot {
    slot_state: AtomicU8,
    cancelled: AtomicBool,
    done: AtomicBool,
    waker: UnsafeCell<Option<Waker>>,
    payload: UnsafeCell<Option<Start>>,
}

// SAFETY: `payload` is written exclusively by the single sender before `SLOT_READY` (and cleared by the sender
// only if it observes `SLOT_CLOSED`), and read exclusively by the single receiver after `SLOT_READY`.
// `waker` is written exclusively by the single receiver while holding `SLOT_REGISTERING`, and taken exclusively
// by the single sender on `SLOT_WAKER_SET -> SLOT_READY | SLOT_CLOSED` or by the receiver on `SLOT_WAKER_SET -> SLOT_CLOSED`.
unsafe impl Send for ResponseSlot {}
unsafe impl Sync for ResponseSlot {}

impl ResponseSlot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            slot_state: AtomicU8::new(SLOT_EMPTY),
            cancelled: AtomicBool::new(false),
            done: AtomicBool::new(false),
            waker: UnsafeCell::new(None),
            payload: UnsafeCell::new(None),
        })
    }

    /// Transitions the sender side out of `SLOT_EMPTY` or `SLOT_WAKER_SET` into `target` (`SLOT_READY` or `SLOT_CLOSED`),
    /// waking any registered receiver. Returns true if this call completed the transition.
    fn transition_sender(&self, target: u8) -> bool {
        let mut state = self.slot_state.load(Ordering::Acquire);
        loop {
            match state {
                SLOT_EMPTY => {
                    match self.slot_state.compare_exchange_weak(
                        SLOT_EMPTY,
                        target,
                        Ordering::Release,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return true,
                        Err(next) => state = next,
                    }
                }
                SLOT_REGISTERING => {
                    std::hint::spin_loop();
                    state = self.slot_state.load(Ordering::Acquire);
                }
                SLOT_WAKER_SET => {
                    match self.slot_state.compare_exchange_weak(
                        SLOT_WAKER_SET,
                        target,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            // SAFETY: we exclusively transitioned out of `SLOT_WAKER_SET`, so the receiver will not touch `waker`.
                            if let Some(w) = unsafe { (*self.waker.get()).take() } {
                                w.wake();
                            }
                            return true;
                        }
                        Err(next) => state = next,
                    }
                }
                _ => return false,
            }
        }
    }

    /// Sends the initial response `Start` once from the PHP thread and wakes the waiting IO shard task if parked.
    fn send(&self, start: Start) -> Result<(), ()> {
        // SAFETY: sole sender; receiver never reads `payload` until `SLOT_READY`.
        unsafe { *self.payload.get() = Some(start) };
        if self.transition_sender(SLOT_READY) {
            Ok(())
        } else {
            // SAFETY: receiver transitioned to `SLOT_CLOSED` without reading `payload`, so sender reclaims it.
            unsafe { *self.payload.get() = None };
            Err(())
        }
    }

    /// Closes the sender half if `HttpResponder` drops without sending `Start`.
    fn close_sender(&self) {
        let _ = self.transition_sender(SLOT_CLOSED);
    }

    /// Polls for the initial `Start` message on the IO shard task.
    pub(crate) fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<Start, ()>> {
        let mut state = self.slot_state.load(Ordering::Acquire);
        loop {
            match state {
                SLOT_READY => {
                    // SAFETY: sender finished writing `payload` before releasing `SLOT_READY`.
                    let start = unsafe { (*self.payload.get()).take() };
                    self.slot_state.store(SLOT_CLOSED, Ordering::Relaxed);
                    return Poll::Ready(start.ok_or(()));
                }
                SLOT_CLOSED => return Poll::Ready(Err(())),
                SLOT_EMPTY | SLOT_WAKER_SET => {
                    match self.slot_state.compare_exchange_weak(
                        state,
                        SLOT_REGISTERING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            // SAFETY: sole receiver holds `SLOT_REGISTERING`; sender spins until `SLOT_WAKER_SET`.
                            let old_waker = unsafe {
                                let slot = &mut *self.waker.get();
                                match slot {
                                    Some(existing) if existing.will_wake(cx.waker()) => None,
                                    _ => slot.replace(cx.waker().clone()),
                                }
                            };
                            self.slot_state.store(SLOT_WAKER_SET, Ordering::Release);
                            drop(old_waker);
                            return Poll::Pending;
                        }
                        Err(next) => state = next,
                    }
                }
                _ => unreachable!("invalid ResponseSlot state"),
            }
        }
    }

    /// Closes the receiver half if `handle_request` drops before observing `SLOT_READY`.
    pub(crate) fn close_receiver(&self) {
        let mut state = self.slot_state.load(Ordering::Acquire);
        loop {
            match state {
                SLOT_EMPTY => {
                    match self.slot_state.compare_exchange_weak(
                        SLOT_EMPTY,
                        SLOT_CLOSED,
                        Ordering::Release,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return,
                        Err(next) => state = next,
                    }
                }
                SLOT_WAKER_SET => {
                    match self.slot_state.compare_exchange_weak(
                        SLOT_WAKER_SET,
                        SLOT_CLOSED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            // SAFETY: we exclusively transitioned out of `SLOT_WAKER_SET`.
                            unsafe { *self.waker.get() = None };
                            return;
                        }
                        Err(next) => state = next,
                    }
                }
                SLOT_READY => {
                    // SAFETY: sender already finished; receiver owns and drops `payload`.
                    unsafe { *self.payload.get() = None };
                    self.slot_state.store(SLOT_CLOSED, Ordering::Relaxed);
                    return;
                }
                _ => return,
            }
        }
    }

    #[inline]
    pub(crate) fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
    }

    #[inline]
    pub(crate) fn cancel_if_incomplete(&self) {
        if !self.done.load(Ordering::Acquire) {
            self.cancelled.store(true, Ordering::Release);
        }
    }

    #[inline]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// First message sent from a PHP worker to the IO shard over `ResponseSlot`.
pub(crate) enum Start {
    /// Complete single-shot response (no `mpsc` channel allocated).
    Complete {
        head: ResponseHead,
        body: Bytes,
        truncated: bool,
    },
    /// Streamed response (created lazily only on `flush()`, multi-chunk writes, or `sendFile()`).
    Stream {
        head: ResponseHead,
        rx: mpsc::Receiver<Chunk>,
    },
    /// Host error response before any head went out (`500` / `503`).
    Error(u16),
}

pub(crate) enum Chunk {
    Data(Bytes),
    File { file: File, offset: u64, len: u64 },
    End { truncated: bool },
}

/// PHP-side response sink for both dispatcher mode (`ExchangeState`) and classic/worker mode (`CgiResponder`).
pub(crate) struct HttpResponder {
    slot: Arc<ResponseSlot>,
    sent_start: bool,
    stream: Option<mpsc::Sender<Chunk>>,
    bodiless: bool,
}

impl HttpResponder {
    pub(crate) fn pair(bodiless: bool) -> (Self, Arc<ResponseSlot>) {
        let slot = ResponseSlot::new();
        let tx = Self {
            slot: Arc::clone(&slot),
            sent_start: false,
            stream: None,
            bodiless,
        };
        (tx, slot)
    }

    pub(super) fn send_start(&mut self, start: Start) -> Result<(), ()> {
        if self.sent_start {
            return Err(());
        }
        self.sent_start = true;
        self.slot.send(start)
    }

    #[inline]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.slot.is_cancelled()
    }

    /// Sends a one-shot response without allocating a stream channel.
    pub(crate) fn write_complete(
        &mut self,
        head: ResponseHead,
        body: &[u8],
        truncated: bool,
    ) -> Result<(), ()> {
        let bodiless = self.bodiless || super::is_bodiless_status(head.status);
        let bytes = if bodiless {
            Bytes::new()
        } else {
            copy_response_bytes(body)
        };
        self.send_start(Start::Complete {
            head,
            body: bytes,
            truncated,
        })
    }

    /// Commits the response head. Bodiless heads complete on the one-shot path; streamed heads open a bounded channel.
    pub(crate) fn write_head(&mut self, head: ResponseHead) -> Result<(), ()> {
        if self.bodiless || super::is_bodiless_status(head.status) {
            self.bodiless = true;
            return self.send_start(Start::Complete {
                head,
                body: Bytes::new(),
                truncated: false,
            });
        }
        let (chunk_tx, chunk_rx) = mpsc::channel(STREAM_CHANNEL_CAP);
        self.stream = Some(chunk_tx);
        self.send_start(Start::Stream { head, rx: chunk_rx })
    }

    /// # Safety
    /// Must run on a PHP worker thread inside the Tokio handle context (`rapira_sapi::stream::send`).
    pub(crate) unsafe fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), ()> {
        if self.bodiless || chunk.is_empty() {
            return if self.is_cancelled() { Err(()) } else { Ok(()) };
        }
        let Some(tx) = self.stream.as_ref() else {
            return Err(());
        };
        let bytes = copy_response_bytes(chunk);
        if unsafe { rapira_sapi::stream::send(tx, Chunk::Data(bytes), None) } {
            Ok(())
        } else {
            Err(())
        }
    }

    /// # Safety
    /// Must run on a PHP worker thread inside the Tokio handle context (`rapira_sapi::stream::send`).
    pub(crate) unsafe fn send_file(&mut self, file: File, offset: u64, len: u64) -> Result<(), ()> {
        if self.bodiless || len == 0 {
            return if self.is_cancelled() { Err(()) } else { Ok(()) };
        }
        let Some(tx) = self.stream.as_ref() else {
            return Err(());
        };
        if unsafe { rapira_sapi::stream::send(tx, Chunk::File { file, offset, len }, None) } {
            Ok(())
        } else {
            Err(())
        }
    }

    /// # Safety
    /// Must run on a PHP worker thread inside the Tokio handle context (`rapira_sapi::stream::send`).
    pub(crate) unsafe fn finish(&mut self, truncated: bool) {
        if let Some(tx) = self.stream.take() {
            let _ = unsafe { rapira_sapi::stream::send(&tx, Chunk::End { truncated }, None) };
        }
    }

    pub(crate) fn send_error(&mut self, status: u16) {
        let _ = self.send_start(Start::Error(status));
    }

    pub(crate) fn discard(&mut self) {
        if !self.sent_start {
            self.sent_start = true;
            self.slot.close_sender();
        }
        self.stream = None;
    }
}

impl Drop for HttpResponder {
    fn drop(&mut self) {
        if !self.sent_start {
            self.slot.close_sender();
        }
    }
}

impl CgiResponder for HttpResponder {
    fn is_cancelled(&self) -> bool {
        self.slot.is_cancelled()
    }

    fn respond(
        mut self: Box<Self>,
        head: rapira_sapi::types::ResponseHead,
        body: Vec<u8>,
        truncated: bool,
    ) {
        let bodiless = self.bodiless || super::is_bodiless_status(head.status);
        let known_len = if bodiless {
            head.headers
                .get(CONTENT_LENGTH)
                .and_then(|v| crate::intake::parse_content_length(v.as_bytes().trim_ascii()))
        } else if truncated {
            None
        } else {
            Some(body.len() as u64)
        };
        let head = ResponseHead {
            status: head.status,
            headers: head.headers,
            known_len,
        };
        let bytes = if bodiless {
            Bytes::new()
        } else {
            Bytes::from(body)
        };
        let _ = self.send_start(Start::Complete {
            head,
            body: bytes,
            truncated,
        });
    }
}
