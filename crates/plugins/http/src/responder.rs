//! PHP-to-IO-shard response handoff, merged single-Arc response slot, recycled large buffers, and unboxed `http_body::Body`.

use std::cell::UnsafeCell;
use std::fmt;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::unix::fs::FileExt;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use http::header::{
    CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, HeaderName, HeaderValue, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use http::{Extensions, HeaderMap, Response, StatusCode};
use http_body::{Body, Frame, SizeHint};
use rapira_http_middleware::{
    BodyEncoder, BodyStream, MiddlewareChain, ResponseHead, ShortResponse,
};
use rapira_sapi::CgiResponder;
use tokio::sync::mpsc;

use crate::io::ConnPhase;
use crate::serve::LocalInflight;

const STREAM_CHANNEL_CAP: usize = 16;
const SENDFILE_SLICE: usize = 256 << 10;
const POOL_MIN_BYTES: usize = 32 << 10;
const POOL_MAX_BYTES: usize = 4 << 20;
const POOL_MAX_BUFS: usize = 32;

static BUF_POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Owned buffer returned to `BUF_POOL` when `hyper` drops the `Bytes` after `writev()`.
struct PooledBuf {
    buf: Vec<u8>,
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

fn take_pooled_buf(min_cap: usize) -> Vec<u8> {
    if let Ok(mut pool) = BUF_POOL.lock() {
        if let Some(pos) = pool.iter().position(|b| b.capacity() >= min_cap) {
            return pool.swap_remove(pos);
        }
        if let Some(mut b) = pool.pop() {
            b.reserve(min_cap);
            return b;
        }
    }
    Vec::with_capacity(min_cap)
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

    /// Sends the initial response `Start` once from the PHP thread and wakes the waiting IO shard task if parked.
    fn send(&self, start: Start) -> Result<(), ()> {
        // SAFETY: sole sender; receiver never reads `payload` until `SLOT_READY`.
        unsafe { *self.payload.get() = Some(start) };
        let mut state = self.slot_state.load(Ordering::Acquire);
        loop {
            match state {
                SLOT_EMPTY => {
                    match self.slot_state.compare_exchange_weak(
                        SLOT_EMPTY,
                        SLOT_READY,
                        Ordering::Release,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Ok(()),
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
                        SLOT_READY,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            // SAFETY: we exclusively transitioned out of `SLOT_WAKER_SET`, so the receiver will not touch `waker`.
                            let waker = unsafe { (*self.waker.get()).take() };
                            if let Some(w) = waker {
                                w.wake();
                            }
                            return Ok(());
                        }
                        Err(next) => state = next,
                    }
                }
                SLOT_CLOSED => {
                    // SAFETY: receiver transitioned to `SLOT_CLOSED` without reading `payload`, so sender reclaims it.
                    unsafe { *self.payload.get() = None };
                    return Err(());
                }
                _ => {
                    unsafe { *self.payload.get() = None };
                    return Err(());
                }
            }
        }
    }

    /// Closes the sender half if `HttpResponder` drops without sending `Start`.
    fn close_sender(&self) {
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
                SLOT_REGISTERING => {
                    std::hint::spin_loop();
                    state = self.slot_state.load(Ordering::Acquire);
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
                            let waker = unsafe { (*self.waker.get()).take() };
                            if let Some(w) = waker {
                                w.wake();
                            }
                            return;
                        }
                        Err(next) => state = next,
                    }
                }
                SLOT_READY | SLOT_CLOSED => return,
                _ => return,
            }
        }
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
                SLOT_CLOSED => return,
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
        content_length: Option<u64>,
        bodiless: bool,
        truncated: bool,
    },
    /// Streamed response (created lazily only on `flush()`, multi-chunk writes, or `sendFile()`).
    Stream {
        head: ResponseHead,
        content_length: Option<u64>,
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

    fn send_start(&mut self, start: Start) -> Result<(), ()> {
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
        content_length: Option<u64>,
        bodiless: bool,
        truncated: bool,
    ) -> Result<(), ()> {
        let bodiless = self.bodiless || bodiless;
        let bytes = if bodiless {
            Bytes::new()
        } else {
            copy_response_bytes(body)
        };
        self.send_start(Start::Complete {
            head,
            body: bytes,
            content_length,
            bodiless,
            truncated,
        })
    }

    /// Commits the response head. Bodiless heads complete on the one-shot path; streamed heads open a bounded channel.
    pub(crate) fn write_head(
        &mut self,
        head: ResponseHead,
        content_length: Option<u64>,
        bodiless: bool,
    ) -> Result<(), ()> {
        if self.bodiless || bodiless {
            self.bodiless = true;
            return self.send_start(Start::Complete {
                head,
                body: Bytes::new(),
                content_length,
                bodiless: true,
                truncated: false,
            });
        }
        let (chunk_tx, chunk_rx) = mpsc::channel(STREAM_CHANNEL_CAP);
        self.stream = Some(chunk_tx);
        self.send_start(Start::Stream {
            head,
            content_length,
            rx: chunk_rx,
        })
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
        let head = ResponseHead {
            status: head.status,
            headers: head.headers,
            known_len: None,
        };
        let status = head.status;
        let bodiless = self.bodiless || matches!(status, 204 | 304 | 101) || status < 200;
        let content_length = if bodiless {
            head.headers
                .get(CONTENT_LENGTH)
                .and_then(|v| crate::check::parse_content_length(v.as_bytes().trim_ascii()))
        } else if truncated {
            None
        } else {
            Some(body.len() as u64)
        };
        let bytes = if bodiless {
            Bytes::new()
        } else {
            Bytes::from(body)
        };
        let _ = self.send_start(Start::Complete {
            head,
            body: bytes,
            content_length,
            bodiless,
            truncated,
        });
    }
}

/// `!Send` guard tied to one request on an IO shard; owns the single IO-side `Arc<ResponseSlot>`,
/// resets `ConnPhase`, decrements shard inflight, and closes/cancels incomplete PHP units on drop.
pub(crate) struct RequestGuard {
    phase: Rc<ConnPhase>,
    inflight: Rc<LocalInflight>,
    slot: Option<Arc<ResponseSlot>>,
    received: bool,
}

impl RequestGuard {
    pub(crate) fn new(phase: Rc<ConnPhase>, inflight: Rc<LocalInflight>) -> Self {
        phase.enter_request();
        inflight.inc();
        Self {
            phase,
            inflight,
            slot: None,
            received: false,
        }
    }

    pub(crate) fn attach_slot(&mut self, slot: Arc<ResponseSlot>) {
        self.slot = Some(slot);
        self.received = false;
    }

    /// Awaits the initial `Start` message directly on the attached `ResponseSlot` without cloning `Arc`.
    pub(crate) async fn wait_start(&mut self) -> Result<Start, ()> {
        std::future::poll_fn(|cx| {
            let slot = self.slot.as_ref().expect("slot attached before wait_start");
            match slot.poll_recv(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(out) => {
                    self.received = true;
                    Poll::Ready(out)
                }
            }
        })
        .await
    }

    pub(crate) fn mark_done(&self) {
        if let Some(slot) = &self.slot {
            slot.mark_done();
        }
    }

    /// Hands completion ownership to `ConnPhase` so `ShardIo::poll_flush` marks PHP done only after the final bytes flush to the socket.
    pub(crate) fn arm_complete(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.phase.arm_complete(slot);
        }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some(slot) = &self.slot {
            if !self.received {
                slot.close_receiver();
            }
            slot.cancel_if_incomplete();
        }
        self.phase.leave_request();
        self.inflight.dec();
    }
}

/// Error returned from `ResponseBody::poll_frame` so `hyper` drops the connection without writing a chunk terminator.
#[derive(Debug)]
pub(crate) struct StreamAbort;

impl fmt::Display for StreamAbort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("response stream aborted")
    }
}

impl std::error::Error for StreamAbort {}

type FileReadFut = Pin<Box<dyn Future<Output = Option<(File, Vec<u8>, io::Result<usize>)>> + Send>>;

struct ActiveFile {
    file: Option<File>,
    offset: u64,
    remaining: u64,
    in_flight: Option<FileReadFut>,
}

impl ActiveFile {
    fn new(file: File, offset: u64, len: u64) -> Self {
        Self {
            file: Some(file),
            offset,
            remaining: len,
            in_flight: None,
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, StreamAbort>>> {
        loop {
            if let Some(fut) = &mut self.in_flight {
                let out = match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(out) => {
                        self.in_flight = None;
                        out
                    }
                };
                let Some((file, mut buf, res)) = out else {
                    return Poll::Ready(Some(Err(StreamAbort)));
                };
                self.file = Some(file);
                match res {
                    Ok(0) => {
                        tracing::warn!(target: "http", "the file shrank mid-send");
                        return Poll::Ready(Some(Err(StreamAbort)));
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        self.offset += n as u64;
                        self.remaining -= n as u64;
                        return Poll::Ready(Some(Ok(Bytes::from_owner(PooledBuf { buf }))));
                    }
                    Err(e) => {
                        tracing::warn!(target: "http", "sendFile read failed: {e}");
                        return Poll::Ready(Some(Err(StreamAbort)));
                    }
                }
            }
            if self.remaining == 0 {
                return Poll::Ready(None);
            }
            let Some(file) = self.file.take() else {
                return Poll::Ready(None);
            };
            let want = (self.remaining as usize).min(SENDFILE_SLICE);
            let offset = self.offset;
            let mut buf = take_pooled_buf(want);
            buf.resize(want, 0);
            self.in_flight = Some(Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    let res = file.read_at(&mut buf, offset);
                    (file, buf, res)
                })
                .await
                .ok()
            }));
        }
    }
}

pub(crate) struct StreamBody {
    rx: mpsc::Receiver<Chunk>,
    encoders: Vec<Box<dyn BodyEncoder>>,
    declared_len: Option<u64>,
    sent_raw: u64,
    active_file: Option<ActiveFile>,
    pending_error: bool,
    finished: bool,
    guard: RequestGuard,
}

impl StreamBody {
    fn encode_chunk(&mut self, data: &[u8]) -> Result<Bytes, StreamAbort> {
        if self.encoders.is_empty() {
            unreachable!("encode_chunk requires active encoders");
        }
        encode_slice(&mut self.encoders, data, true).map_err(|_| StreamAbort)
    }

    fn finish_encoders(&mut self) -> Result<Bytes, StreamAbort> {
        if self.encoders.is_empty() {
            return Ok(Bytes::new());
        }
        finish_encoders(&mut self.encoders).map_err(|_| StreamAbort)
    }

    fn trigger_abort(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, StreamAbort>>> {
        if !self.pending_error {
            // Yield once so hyper flushes any previously buffered chunk before dropping the socket.
            self.pending_error = true;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.finished = true;
        Poll::Ready(Some(Err(StreamAbort)))
    }
}

impl Body for StreamBody {
    type Data = Bytes;
    type Error = StreamAbort;

    fn is_end_stream(&self) -> bool {
        self.finished
    }

    fn size_hint(&self) -> SizeHint {
        if self.encoders.is_empty()
            && let Some(len) = self.declared_len
        {
            let rem = len.saturating_sub(self.sent_raw);
            return SizeHint::with_exact(rem);
        }
        SizeHint::default()
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        if this.pending_error {
            this.finished = true;
            return Poll::Ready(Some(Err(StreamAbort)));
        }

        loop {
            if let Some(af) = &mut this.active_file {
                match af.poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(Ok(chunk))) => {
                        this.sent_raw += chunk.len() as u64;
                        let reached_declared = this.declared_len == Some(this.sent_raw);
                        if this.encoders.is_empty() {
                            if reached_declared {
                                this.active_file = None;
                                this.guard.arm_complete();
                                this.finished = true;
                            }
                            return Poll::Ready(Some(Ok(Frame::data(chunk))));
                        }
                        match this.encode_chunk(&chunk) {
                            Ok(encoded) => {
                                if reached_declared {
                                    this.active_file = None;
                                    this.guard.arm_complete();
                                    let tail = match this.finish_encoders() {
                                        Ok(t) => t,
                                        Err(_) => return this.trigger_abort(cx),
                                    };
                                    this.finished = true;
                                    let combined = concat_bytes(encoded, tail);
                                    if !combined.is_empty() {
                                        return Poll::Ready(Some(Ok(Frame::data(combined))));
                                    }
                                    return Poll::Ready(None);
                                }
                                if !encoded.is_empty() {
                                    return Poll::Ready(Some(Ok(Frame::data(encoded))));
                                }
                                continue;
                            }
                            Err(_) => return this.trigger_abort(cx),
                        }
                    }
                    Poll::Ready(Some(Err(_))) => {
                        this.active_file = None;
                        return this.trigger_abort(cx);
                    }
                    Poll::Ready(None) => {
                        this.active_file = None;
                        continue;
                    }
                }
            }

            match this.rx.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return this.trigger_abort(cx),
                Poll::Ready(Some(Chunk::Data(chunk))) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    this.sent_raw += chunk.len() as u64;
                    let reached_declared = this.declared_len == Some(this.sent_raw);
                    if this.encoders.is_empty() {
                        if reached_declared {
                            this.guard.arm_complete();
                            this.finished = true;
                        }
                        return Poll::Ready(Some(Ok(Frame::data(chunk))));
                    }
                    match this.encode_chunk(&chunk) {
                        Ok(encoded) => {
                            if reached_declared {
                                this.guard.arm_complete();
                                let tail = match this.finish_encoders() {
                                    Ok(t) => t,
                                    Err(_) => return this.trigger_abort(cx),
                                };
                                this.finished = true;
                                let combined = concat_bytes(encoded, tail);
                                if !combined.is_empty() {
                                    return Poll::Ready(Some(Ok(Frame::data(combined))));
                                }
                                return Poll::Ready(None);
                            }
                            if !encoded.is_empty() {
                                return Poll::Ready(Some(Ok(Frame::data(encoded))));
                            }
                        }
                        Err(_) => return this.trigger_abort(cx),
                    }
                }
                Poll::Ready(Some(Chunk::File { file, offset, len })) => {
                    if len > 0 {
                        this.active_file = Some(ActiveFile::new(file, offset, len));
                    }
                }
                Poll::Ready(Some(Chunk::End { truncated })) => {
                    if truncated || this.declared_len.is_some_and(|cl| this.sent_raw < cl) {
                        return this.trigger_abort(cx);
                    }
                    this.guard.arm_complete();
                    this.finished = true;
                    if !this.encoders.is_empty() {
                        match this.finish_encoders() {
                            Ok(tail) if !tail.is_empty() => {
                                return Poll::Ready(Some(Ok(Frame::data(tail))));
                            }
                            Ok(_) => {}
                            Err(_) => return this.trigger_abort(cx),
                        }
                    }
                    return Poll::Ready(None);
                }
            }
        }
    }
}

pub(crate) struct MiddlewareBody {
    first: Option<Bytes>,
    rest: Option<Box<dyn BodyStream>>,
    encoders: Vec<Box<dyn BodyEncoder>>,
    known_len: Option<u64>,
    sent_raw: u64,
    finished: bool,
    _guard: RequestGuard,
}

impl Body for MiddlewareBody {
    type Data = Bytes;
    type Error = StreamAbort;

    fn is_end_stream(&self) -> bool {
        self.finished
    }

    fn size_hint(&self) -> SizeHint {
        if self.encoders.is_empty()
            && let Some(len) = self.known_len
        {
            return SizeHint::with_exact(len.saturating_sub(self.sent_raw));
        }
        SizeHint::default()
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        loop {
            if let Some(first) = this.first.take() {
                if first.is_empty() {
                    continue;
                }
                this.sent_raw += first.len() as u64;
                if this.encoders.is_empty() {
                    if this.rest.is_none() {
                        this.finished = true;
                    }
                    return Poll::Ready(Some(Ok(Frame::data(first))));
                }
                match encode_slice(&mut this.encoders, &first, true) {
                    Ok(encoded) => {
                        if this.rest.is_none() {
                            this.finished = true;
                            let tail = match finish_encoders(&mut this.encoders) {
                                Ok(t) => t,
                                Err(_) => return Poll::Ready(Some(Err(StreamAbort))),
                            };
                            let combined = concat_bytes(encoded, tail);
                            if !combined.is_empty() {
                                return Poll::Ready(Some(Ok(Frame::data(combined))));
                            }
                            return Poll::Ready(None);
                        }
                        if !encoded.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(encoded))));
                        }
                        continue;
                    }
                    Err(_) => {
                        this.finished = true;
                        return Poll::Ready(Some(Err(StreamAbort)));
                    }
                }
            }

            let Some(stream) = &mut this.rest else {
                this.finished = true;
                return Poll::Ready(None);
            };
            match Pin::new(stream.as_mut()).poll_next_chunk(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.rest = None;
                    this.finished = true;
                    if !this.encoders.is_empty() {
                        match finish_encoders(&mut this.encoders) {
                            Ok(tail) if !tail.is_empty() => {
                                return Poll::Ready(Some(Ok(Frame::data(tail))));
                            }
                            Ok(_) => {}
                            Err(_) => return Poll::Ready(Some(Err(StreamAbort))),
                        }
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    this.sent_raw += chunk.len() as u64;
                    if this.encoders.is_empty() {
                        return Poll::Ready(Some(Ok(Frame::data(chunk))));
                    }
                    match encode_slice(&mut this.encoders, &chunk, true) {
                        Ok(encoded) if !encoded.is_empty() => {
                            return Poll::Ready(Some(Ok(Frame::data(encoded))));
                        }
                        Ok(_) => continue,
                        Err(_) => {
                            this.finished = true;
                            return Poll::Ready(Some(Err(StreamAbort)));
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    tracing::error!(target: "http", "middleware stream read failed: {e}");
                    this.finished = true;
                    return Poll::Ready(Some(Err(StreamAbort)));
                }
            }
        }
    }
}

/// Unboxed concrete `http_body::Body` returned to `hyper`.
pub(crate) enum ResponseBody {
    Empty {
        _guard: RequestGuard,
    },
    Once {
        data: Option<Bytes>,
        _guard: RequestGuard,
    },
    Truncated {
        data: Option<Bytes>,
        declared_len: Option<u64>,
        pending_error: bool,
        finished: bool,
        _guard: RequestGuard,
    },
    Stream(Box<StreamBody>),
    Middleware(Box<MiddlewareBody>),
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = StreamAbort;

    #[inline]
    fn is_end_stream(&self) -> bool {
        match self {
            Self::Empty { .. } => true,
            Self::Once { data, .. } => data.is_none(),
            Self::Truncated { finished, .. } => *finished,
            Self::Stream(s) => s.is_end_stream(),
            Self::Middleware(m) => m.is_end_stream(),
        }
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Empty { .. } => SizeHint::with_exact(0),
            Self::Once { data, .. } => {
                SizeHint::with_exact(data.as_ref().map_or(0, |b| b.len() as u64))
            }
            Self::Truncated { declared_len, .. } => match declared_len {
                Some(cl) => SizeHint::with_exact(*cl),
                None => SizeHint::default(),
            },
            Self::Stream(s) => s.size_hint(),
            Self::Middleware(m) => m.size_hint(),
        }
    }

    #[inline]
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            Self::Empty { .. } => Poll::Ready(None),
            Self::Once { data, .. } => match data.take() {
                Some(b) if !b.is_empty() => Poll::Ready(Some(Ok(Frame::data(b)))),
                _ => Poll::Ready(None),
            },
            Self::Truncated {
                data,
                pending_error,
                finished,
                ..
            } => {
                if *finished {
                    return Poll::Ready(None);
                }
                if let Some(b) = data.take()
                    && !b.is_empty()
                {
                    return Poll::Ready(Some(Ok(Frame::data(b))));
                }
                if !*pending_error {
                    *pending_error = true;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                *finished = true;
                Poll::Ready(Some(Err(StreamAbort)))
            }
            Self::Stream(s) => Pin::new(s.as_mut()).poll_frame(cx),
            Self::Middleware(m) => Pin::new(m.as_mut()).poll_frame(cx),
        }
    }
}

#[inline]
fn is_hop_by_hop(name: &HeaderName) -> bool {
    *name == CONTENT_LENGTH
        || *name == TRANSFER_ENCODING
        || *name == CONNECTION
        || *name == UPGRADE
        || *name == TRAILER
        || *name == TE
        || name.as_str() == "keep-alive"
        || name.as_str() == "proxy-connection"
}

/// Strips hop-by-hop headers (RFC 9110 §7.6.1) plus any header names listed in `Connection`.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    if headers.is_empty() || !headers.keys().any(is_hop_by_hop) {
        return;
    }
    const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
    const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");
    if headers.contains_key(CONNECTION) {
        let mut nominated: Vec<HeaderName> = Vec::new();
        for val in headers.get_all(CONNECTION) {
            if let Ok(text) = val.to_str() {
                for token in text.split(',') {
                    let token = token.trim();
                    if !token.is_empty()
                        && let Ok(name) = HeaderName::from_bytes(token.as_bytes())
                    {
                        nominated.push(name);
                    }
                }
            }
        }
        for name in nominated {
            headers.remove(&name);
        }
    }
    headers.remove(CONTENT_LENGTH);
    headers.remove(TRANSFER_ENCODING);
    headers.remove(CONNECTION);
    headers.remove(KEEP_ALIVE);
    headers.remove(UPGRADE);
    headers.remove(TRAILER);
    headers.remove(TE);
    headers.remove(PROXY_CONNECTION);
}

fn concat_bytes(a: Bytes, b: Bytes) -> Bytes {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    out.extend_from_slice(&a);
    out.extend_from_slice(&b);
    Bytes::from(out)
}

fn encode_slice(
    encoders: &mut [Box<dyn BodyEncoder>],
    input: &[u8],
    flush: bool,
) -> io::Result<Bytes> {
    if encoders.len() == 1 {
        let mut out = Vec::new();
        encoders[0].write(input, &mut out)?;
        if flush {
            encoders[0].flush(&mut out)?;
        }
        return Ok(Bytes::from(out));
    }
    let mut cur = input.to_vec();
    let mut next = Vec::new();
    for enc in encoders.iter_mut() {
        next.clear();
        enc.write(&cur, &mut next)?;
        if flush {
            enc.flush(&mut next)?;
        }
        std::mem::swap(&mut cur, &mut next);
    }
    Ok(Bytes::from(cur))
}

fn finish_encoders(encoders: &mut [Box<dyn BodyEncoder>]) -> io::Result<Bytes> {
    if encoders.len() == 1 {
        let mut out = Vec::new();
        encoders[0].finish(&mut out)?;
        return Ok(Bytes::from(out));
    }
    let mut cur = Vec::new();
    let mut next = Vec::new();
    for enc in encoders.iter_mut() {
        next.clear();
        if !cur.is_empty() {
            enc.write(&cur, &mut next)?;
        }
        enc.finish(&mut next)?;
        std::mem::swap(&mut cur, &mut next);
    }
    Ok(Bytes::from(cur))
}

fn encode_complete(encoders: &mut [Box<dyn BodyEncoder>], input: &[u8]) -> io::Result<Bytes> {
    if encoders.len() == 1 {
        let mut out = Vec::new();
        if !input.is_empty() {
            encoders[0].write(input, &mut out)?;
        }
        encoders[0].finish(&mut out)?;
        return Ok(Bytes::from(out));
    }
    let mut cur = input.to_vec();
    let mut next = Vec::new();
    for enc in encoders.iter_mut() {
        next.clear();
        if !cur.is_empty() {
            enc.write(&cur, &mut next)?;
        }
        enc.finish(&mut next)?;
        std::mem::swap(&mut cur, &mut next);
    }
    Ok(Bytes::from(cur))
}

/// Builds a standardized host refusal response (`Cache-Control: private, no-store`, `Connection: close`).
pub(crate) fn refusal(status: StatusCode, guard: RequestGuard) -> Response<ResponseBody> {
    guard.mark_done();
    let mut res = Response::new(ResponseBody::Empty { _guard: guard });
    *res.status_mut() = status;
    let hdrs = res.headers_mut();
    hdrs.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    hdrs.insert(CONNECTION, HeaderValue::from_static("close"));
    res
}

/// Converts a Phase-1 middleware `ShortResponse` into a `hyper` response, running outer Phase-2 hooks.
pub(crate) fn build_short_response(
    chain: &MiddlewareChain,
    outer_depth: usize,
    is_head: bool,
    closing: bool,
    extensions: &Extensions,
    mut short: ShortResponse,
    guard: RequestGuard,
) -> Response<ResponseBody> {
    let mut encoders =
        chain.run_response_head(Some(outer_depth), is_head, extensions, &mut short.head);
    let status =
        StatusCode::from_u16(short.head.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let bodiless = is_head || matches!(short.head.status, 204 | 304);
    let mut headers = short.head.headers;
    strip_hop_by_hop(&mut headers);
    if closing {
        headers.insert(CONNECTION, HeaderValue::from_static("close"));
    }

    let body = if bodiless {
        if is_head
            && encoders.is_empty()
            && let Some(len) = short.head.known_len
        {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
        }
        ResponseBody::Empty { _guard: guard }
    } else if short.rest.is_none() {
        let bytes = if encoders.is_empty() {
            short.body
        } else {
            match encode_complete(&mut encoders, &short.body) {
                Ok(b) => b,
                Err(_) => return refusal(StatusCode::INTERNAL_SERVER_ERROR, guard),
            }
        };
        if bytes.is_empty() {
            ResponseBody::Empty { _guard: guard }
        } else {
            ResponseBody::Once {
                data: Some(bytes),
                _guard: guard,
            }
        }
    } else {
        ResponseBody::Middleware(Box::new(MiddlewareBody {
            first: Some(short.body),
            rest: short.rest,
            encoders,
            known_len: short.head.known_len,
            sent_raw: 0,
            finished: false,
            _guard: guard,
        }))
    };

    let mut res = Response::new(body);
    *res.status_mut() = status;
    *res.headers_mut() = headers;
    res
}

/// Converts a PHP `Start` message into a `hyper` `Response<ResponseBody>`.
pub(crate) fn build_php_response(
    chain: &MiddlewareChain,
    is_head: bool,
    closing: bool,
    extensions: &Extensions,
    start: Start,
    mut guard: RequestGuard,
) -> Response<ResponseBody> {
    match start {
        Start::Error(code) => {
            let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            refusal(status, guard)
        }
        Start::Complete {
            mut head,
            body,
            content_length,
            bodiless,
            truncated,
        } => {
            if head.status < 200 {
                guard.arm_complete();
                let mut res = Response::new(ResponseBody::Empty { _guard: guard });
                *res.status_mut() = StatusCode::BAD_GATEWAY;
                if closing || truncated {
                    res.headers_mut()
                        .insert(CONNECTION, HeaderValue::from_static("close"));
                }
                return res;
            }
            let bodiless = bodiless || is_head || matches!(head.status, 204 | 304);
            head.known_len = if bodiless {
                content_length
            } else if truncated {
                None
            } else {
                content_length.or(Some(body.len() as u64))
            };
            let mut encoders = chain.run_response_head(None, is_head, extensions, &mut head);
            let status = StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut headers = head.headers;
            strip_hop_by_hop(&mut headers);
            if closing || (truncated && bodiless) {
                headers.insert(CONNECTION, HeaderValue::from_static("close"));
            }

            let declared_under_run = !bodiless
                && !truncated
                && content_length.is_some_and(|cl| (body.len() as u64) < cl);

            let resp_body = if bodiless {
                guard.arm_complete();
                if is_head
                    && !matches!(head.status, 204 | 304)
                    && let Some(cl) = head.known_len
                {
                    headers.insert(CONTENT_LENGTH, HeaderValue::from(cl));
                }
                ResponseBody::Empty { _guard: guard }
            } else if truncated || declared_under_run {
                let wire = if encoders.is_empty() {
                    body
                } else {
                    encode_slice(&mut encoders, &body, true).unwrap_or_default()
                };
                ResponseBody::Truncated {
                    data: (!wire.is_empty()).then_some(wire),
                    declared_len: (declared_under_run && encoders.is_empty())
                        .then_some(content_length)
                        .flatten(),
                    pending_error: false,
                    finished: false,
                    _guard: guard,
                }
            } else {
                guard.arm_complete();
                let wire = if encoders.is_empty() {
                    body
                } else {
                    match encode_complete(&mut encoders, &body) {
                        Ok(b) => b,
                        Err(_) => return refusal(StatusCode::INTERNAL_SERVER_ERROR, guard),
                    }
                };
                if wire.is_empty() {
                    ResponseBody::Empty { _guard: guard }
                } else {
                    ResponseBody::Once {
                        data: Some(wire),
                        _guard: guard,
                    }
                }
            };

            let mut res = Response::new(resp_body);
            *res.status_mut() = status;
            *res.headers_mut() = headers;
            res
        }
        Start::Stream {
            mut head,
            content_length,
            rx,
        } => {
            if head.status < 200 {
                guard.arm_complete();
                let mut res = Response::new(ResponseBody::Empty { _guard: guard });
                *res.status_mut() = StatusCode::BAD_GATEWAY;
                if closing {
                    res.headers_mut()
                        .insert(CONNECTION, HeaderValue::from_static("close"));
                }
                return res;
            }
            head.known_len = content_length;
            let encoders = chain.run_response_head(None, is_head, extensions, &mut head);
            let status = StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut headers = head.headers;
            strip_hop_by_hop(&mut headers);
            if closing {
                headers.insert(CONNECTION, HeaderValue::from_static("close"));
            }

            let resp_body = if encoders.is_empty() && head.known_len == Some(0) {
                guard.arm_complete();
                ResponseBody::Empty { _guard: guard }
            } else {
                ResponseBody::Stream(Box::new(StreamBody {
                    rx,
                    encoders,
                    declared_len: content_length,
                    sent_raw: 0,
                    active_file: None,
                    pending_error: false,
                    finished: false,
                    guard,
                }))
            };

            let mut res = Response::new(resp_body);
            *res.status_mut() = status;
            *res.headers_mut() = headers;
            res
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use super::*;

    struct CountWaker(AtomicUsize);

    impl Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn count_waker() -> (Arc<CountWaker>, Waker) {
        let counter = Arc::new(CountWaker(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        (counter, waker)
    }

    #[test]
    fn response_slot_interleavings() {
        enum Action {
            SendBeforePoll,
            PollThenSend,
            PollTwiceWithNewWakerThenSend,
            CloseSenderBeforePoll,
            PollThenCloseSender,
            CloseReceiverBeforeSend,
            PollThenCloseReceiverBeforeSend,
            SendThenCloseReceiver,
        }
        struct Case {
            name: &'static str,
            action: Action,
            want_wakes: usize,
        }
        let cases = [
            Case {
                name: "send before poll resolves without waking",
                action: Action::SendBeforePoll,
                want_wakes: 0,
            },
            Case {
                name: "poll before send registers waker and wakes once",
                action: Action::PollThenSend,
                want_wakes: 1,
            },
            Case {
                name: "second poll replaces waker and wakes only the latest",
                action: Action::PollTwiceWithNewWakerThenSend,
                want_wakes: 1,
            },
            Case {
                name: "close sender before poll returns Err without waking",
                action: Action::CloseSenderBeforePoll,
                want_wakes: 0,
            },
            Case {
                name: "close sender after poll wakes receiver and returns Err",
                action: Action::PollThenCloseSender,
                want_wakes: 1,
            },
            Case {
                name: "close receiver before send returns Err to sender",
                action: Action::CloseReceiverBeforeSend,
                want_wakes: 0,
            },
            Case {
                name: "poll then drop receiver before send returns Err without waking",
                action: Action::PollThenCloseReceiverBeforeSend,
                want_wakes: 0,
            },
            Case {
                name: "send then drop receiver reclaims payload cleanly",
                action: Action::SendThenCloseReceiver,
                want_wakes: 0,
            },
        ];

        for case in cases {
            let (mut tx, slot) = HttpResponder::pair(false);
            let (w1_cnt, w1) = count_waker();
            let mut cx1 = Context::from_waker(&w1);
            match case.action {
                Action::SendBeforePoll => {
                    tx.send_error(200);
                    let res = slot.poll_recv(&mut cx1);
                    assert!(
                        matches!(res, Poll::Ready(Ok(Start::Error(200)))),
                        "{}",
                        case.name
                    );
                    assert_eq!(
                        w1_cnt.0.load(Ordering::Relaxed),
                        case.want_wakes,
                        "{}",
                        case.name
                    );
                }
                Action::PollThenSend => {
                    assert!(slot.poll_recv(&mut cx1).is_pending(), "{}", case.name);
                    tx.send_error(201);
                    assert_eq!(
                        w1_cnt.0.load(Ordering::Relaxed),
                        case.want_wakes,
                        "{}",
                        case.name
                    );
                    let res = slot.poll_recv(&mut cx1);
                    assert!(
                        matches!(res, Poll::Ready(Ok(Start::Error(201)))),
                        "{}",
                        case.name
                    );
                }
                Action::PollTwiceWithNewWakerThenSend => {
                    assert!(slot.poll_recv(&mut cx1).is_pending(), "{}", case.name);
                    let (w2_cnt, w2) = count_waker();
                    let mut cx2 = Context::from_waker(&w2);
                    assert!(slot.poll_recv(&mut cx2).is_pending(), "{}", case.name);
                    tx.send_error(202);
                    assert_eq!(w1_cnt.0.load(Ordering::Relaxed), 0, "{}", case.name);
                    assert_eq!(
                        w2_cnt.0.load(Ordering::Relaxed),
                        case.want_wakes,
                        "{}",
                        case.name
                    );
                    let res = slot.poll_recv(&mut cx2);
                    assert!(
                        matches!(res, Poll::Ready(Ok(Start::Error(202)))),
                        "{}",
                        case.name
                    );
                }
                Action::CloseSenderBeforePoll => {
                    drop(tx);
                    let res = slot.poll_recv(&mut cx1);
                    assert!(matches!(res, Poll::Ready(Err(()))), "{}", case.name);
                    assert_eq!(
                        w1_cnt.0.load(Ordering::Relaxed),
                        case.want_wakes,
                        "{}",
                        case.name
                    );
                }
                Action::PollThenCloseSender => {
                    assert!(slot.poll_recv(&mut cx1).is_pending(), "{}", case.name);
                    drop(tx);
                    assert_eq!(
                        w1_cnt.0.load(Ordering::Relaxed),
                        case.want_wakes,
                        "{}",
                        case.name
                    );
                    let res = slot.poll_recv(&mut cx1);
                    assert!(matches!(res, Poll::Ready(Err(()))), "{}", case.name);
                }
                Action::CloseReceiverBeforeSend => {
                    slot.close_receiver();
                    assert!(tx.send_start(Start::Error(200)).is_err(), "{}", case.name);
                    assert_eq!(
                        w1_cnt.0.load(Ordering::Relaxed),
                        case.want_wakes,
                        "{}",
                        case.name
                    );
                }
                Action::PollThenCloseReceiverBeforeSend => {
                    assert!(slot.poll_recv(&mut cx1).is_pending(), "{}", case.name);
                    slot.close_receiver();
                    assert!(tx.send_start(Start::Error(200)).is_err(), "{}", case.name);
                    assert_eq!(
                        w1_cnt.0.load(Ordering::Relaxed),
                        case.want_wakes,
                        "{}",
                        case.name
                    );
                }
                Action::SendThenCloseReceiver => {
                    assert!(tx.send_start(Start::Error(200)).is_ok(), "{}", case.name);
                    slot.close_receiver();
                    assert_eq!(
                        w1_cnt.0.load(Ordering::Relaxed),
                        case.want_wakes,
                        "{}",
                        case.name
                    );
                }
            }
        }
    }

    #[test]
    fn response_slot_cancel_respects_mark_done() {
        struct Case {
            name: &'static str,
            mark_done_first: bool,
            want_cancelled: bool,
        }
        let cases = [
            Case {
                name: "cancel before mark_done sets cancelled",
                mark_done_first: false,
                want_cancelled: true,
            },
            Case {
                name: "cancel after mark_done does not set cancelled",
                mark_done_first: true,
                want_cancelled: false,
            },
        ];
        for case in cases {
            let slot = ResponseSlot::new();
            if case.mark_done_first {
                slot.mark_done();
            }
            slot.cancel_if_incomplete();
            assert_eq!(slot.is_cancelled(), case.want_cancelled, "{}", case.name);
        }
    }

    #[test]
    fn response_slot_cross_thread_stress() {
        for i in 0..1000u16 {
            let (mut tx, slot) = HttpResponder::pair(false);
            let status = 200 + (i % 100);
            let handle = std::thread::spawn(move || {
                tx.send_error(status);
            });
            let (w_cnt, waker) = count_waker();
            let mut cx = Context::from_waker(&waker);
            let got = loop {
                match slot.poll_recv(&mut cx) {
                    Poll::Ready(res) => break res,
                    Poll::Pending => std::thread::yield_now(),
                }
            };
            handle.join().unwrap();
            assert!(matches!(got, Ok(Start::Error(code)) if code == status));
            assert!(w_cnt.0.load(Ordering::Relaxed) <= 1);
        }
    }

    #[test]
    fn compressed_declared_under_run_aborts() {
        use http::header::{ACCEPT_ENCODING, CONTENT_TYPE};
        use rapira_http_middleware::{Middleware, Peer, PeerAddr, RequestCtx};

        struct Case {
            name: &'static str,
            stream: bool,
            declared_len: u64,
            body_len: usize,
            want_abort: bool,
        }
        let cases = [
            Case {
                name: "one-shot compressed body shorter than content-length aborts",
                stream: false,
                declared_len: 100,
                body_len: 50,
                want_abort: true,
            },
            Case {
                name: "streamed compressed body shorter than content-length aborts",
                stream: true,
                declared_len: 100,
                body_len: 50,
                want_abort: true,
            },
            Case {
                name: "one-shot compressed body matching content-length completes",
                stream: false,
                declared_len: 100,
                body_len: 100,
                want_abort: false,
            },
            Case {
                name: "streamed compressed body matching content-length completes",
                stream: true,
                declared_len: 100,
                body_len: 100,
                want_abort: false,
            },
        ];

        let compress = Arc::new(rapira_compress::Compress::new(
            rapira_compress::resolve(rapira_compress::Section {
                min_size: Some(16),
                ..Default::default()
            })
            .unwrap(),
        ));
        let chain = MiddlewareChain::new(vec![compress.clone() as Arc<dyn Middleware>]);

        let mut req_headers = HeaderMap::new();
        req_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        let mut ctx = RequestCtx {
            method: http::Method::GET,
            uri: http::Uri::from_static("/"),
            version: http::Version::HTTP_11,
            headers: req_headers,
            peer: Peer {
                remote: PeerAddr::Inet(([127, 0, 0, 1], 40000).into()),
                server: PeerAddr::Inet(([127, 0, 0, 1], 8000).into()),
                https: false,
                received_at: 0.0,
            },
            extensions: Extensions::new(),
        };
        let _ = compress.on_request(&mut ctx);

        for case in cases {
            let mut head = ResponseHead::new(200);
            head.headers
                .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            let start = if case.stream {
                let (tx, rx) = mpsc::channel(4);
                tx.try_send(Chunk::Data(Bytes::from(vec![b'x'; case.body_len])))
                    .unwrap();
                tx.try_send(Chunk::End { truncated: false }).unwrap();
                Start::Stream {
                    head,
                    content_length: Some(case.declared_len),
                    rx,
                }
            } else {
                Start::Complete {
                    head,
                    body: Bytes::from(vec![b'x'; case.body_len]),
                    content_length: Some(case.declared_len),
                    bodiless: false,
                    truncated: false,
                }
            };
            let guard = RequestGuard::new(ConnPhase::new(), Rc::new(LocalInflight::default()));
            let resp = build_php_response(&chain, false, false, &ctx.extensions, start, guard);
            let mut body = resp.into_body();
            let (_w_cnt, waker) = count_waker();
            let mut cx = Context::from_waker(&waker);
            let mut aborted = false;
            loop {
                match Pin::new(&mut body).poll_frame(&mut cx) {
                    Poll::Ready(Some(Ok(_))) => {}
                    Poll::Ready(Some(Err(_))) => {
                        aborted = true;
                        break;
                    }
                    Poll::Ready(None) => break,
                    Poll::Pending => {}
                }
            }
            assert_eq!(aborted, case.want_abort, "{}", case.name);
        }
    }
}
