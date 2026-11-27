//! HTTP connection driver: `ShardIo<S>` timeouts, `ConnPhase` flush completion, `Inflight` tracking, and `TrackBody<B>`.

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use http::{Response, StatusCode};
use http_body::{Body, Frame, SizeHint};
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::{Instant, Sleep};

use crate::responder::{ResponseBody, ResponseSlot, Start, refusal};

/// Nonblocking stream accepted by `rapira_net` and registered on one shard's Tokio reactor.
pub(crate) enum RawIo {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl RawIo {
    pub(crate) fn from_tcp(stream: std::net::TcpStream) -> io::Result<Self> {
        Ok(Self::Tcp(TcpStream::from_std(stream)?))
    }

    pub(crate) fn from_unix(stream: std::os::unix::net::UnixStream) -> io::Result<Self> {
        Ok(Self::Unix(UnixStream::from_std(stream)?))
    }
}

impl AsyncRead for RawIo {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RawIo {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Self::Unix(s) => Pin::new(s).poll_write_vectored(cx, bufs),
        }
    }
}

/// Cache-line-padded inflight request counter owned by one IO shard.
/// Only the owning IO shard writes to it, so plain stores avoid atomic RMW and cross-shard false sharing.
#[repr(align(64))]
#[derive(Default)]
pub(crate) struct InflightShard {
    count: AtomicUsize,
}

impl InflightShard {
    #[inline]
    pub(crate) fn store(&self, n: usize) {
        self.count.store(n, Ordering::Release);
    }

    #[inline]
    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

/// Sharded inflight request tracker across all IO shards.
pub(crate) struct Inflight {
    shards: Vec<Arc<InflightShard>>,
}

impl Inflight {
    pub(crate) fn with_shards(n: usize) -> Self {
        let n = n.max(1);
        Self {
            shards: (0..n).map(|_| Arc::new(InflightShard::default())).collect(),
        }
    }

    pub(crate) fn shard_at(&self, idx: usize) -> Arc<InflightShard> {
        Arc::clone(&self.shards[idx % self.shards.len()])
    }

    pub(crate) fn count(&self) -> usize {
        self.shards.iter().map(|s| s.count()).sum()
    }
}

/// `!Send` per-shard inflight counter backed by a cache-padded atomic read during drain.
#[derive(Default)]
pub(crate) struct LocalInflight {
    atomic: Arc<InflightShard>,
    local: Cell<usize>,
}

impl LocalInflight {
    pub(crate) fn new(atomic: Arc<InflightShard>) -> Self {
        Self {
            atomic,
            local: Cell::new(0),
        }
    }

    #[inline]
    pub(crate) fn inc(&self) {
        let next = self.local.get() + 1;
        self.local.set(next);
        self.atomic.store(next);
    }

    #[inline]
    pub(crate) fn dec(&self) {
        let next = self.local.get() - 1;
        self.local.set(next);
        self.atomic.store(next);
    }
}

/// Connection lifecycle phase shared on the `!Send` shard between `ShardIo` and the request handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoPhase {
    /// Idle between keep-alive requests or reading a request head; `keepalive_timeout` applies to `poll_read`.
    ReadHead,
    /// Active request body read, PHP execution, or response streaming; `poll_read` is not timed by `ShardIo`.
    InRequest,
}

/// Shared per-connection state mutated without atomics on the shard's `LocalSet`.
pub(crate) struct ConnPhase {
    phase: Cell<IoPhase>,
    /// Epoch counter incremented on every `InRequest -> ReadHead` transition so `ShardIo` re-arms idle sleep once per request.
    epoch: Cell<u64>,
    /// Pending response completion waiting for `ShardIo::poll_flush` to finish writing to the kernel socket.
    pending_complete: Cell<Option<Arc<ResponseSlot>>>,
}

impl ConnPhase {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            phase: Cell::new(IoPhase::ReadHead),
            epoch: Cell::new(0),
            pending_complete: Cell::new(None),
        })
    }

    #[inline]
    pub(crate) fn enter_request(&self) {
        self.phase.set(IoPhase::InRequest);
    }

    #[inline]
    pub(crate) fn leave_request(&self) {
        self.phase.set(IoPhase::ReadHead);
        self.epoch.set(self.epoch.get().wrapping_add(1));
    }

    /// Registers a completed response body so `ShardIo::poll_flush` marks PHP done only after the final byte flushes.
    #[inline]
    pub(crate) fn arm_complete(&self, slot: Arc<ResponseSlot>) {
        if let Some(prev) = self.pending_complete.replace(Some(slot)) {
            prev.cancel_if_incomplete();
        }
    }

    /// Called by `ShardIo::poll_flush` when `hyper`'s `WriteBuf` has drained completely to the socket.
    #[inline]
    pub(crate) fn on_flushed(&self) {
        if let Some(cancel) = self.pending_complete.take() {
            cancel.mark_done();
        }
    }

    /// Cancels any response whose final bytes had not yet flushed when the socket errored or dropped.
    #[inline]
    pub(crate) fn cancel_pending(&self) {
        if let Some(cancel) = self.pending_complete.take() {
            cancel.cancel_if_incomplete();
        }
    }

    #[inline]
    fn current(&self) -> (IoPhase, u64) {
        (self.phase.get(), self.epoch.get())
    }
}

impl Drop for ConnPhase {
    fn drop(&mut self) {
        self.cancel_pending();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeaseState {
    Active,
    AwaitingStart,
    Aborted,
}

/// `!Send` RAII lease held by `RequestScope` while preparing/waiting, then moved into `TrackBody<B>`.
struct RequestLease {
    phase: Rc<ConnPhase>,
    inflight: Rc<LocalInflight>,
    slot: Option<Arc<ResponseSlot>>,
    state: LeaseState,
}

impl RequestLease {
    #[inline]
    fn has_slot(&self) -> bool {
        self.slot.is_some()
    }

    #[inline]
    fn arm_complete(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.phase.arm_complete(slot);
        }
    }

    #[cold]
    #[inline(never)]
    fn cancel_slot(&mut self) {
        if let Some(slot) = self.slot.take() {
            if self.state == LeaseState::AwaitingStart {
                slot.close_receiver();
                self.state = LeaseState::Active;
            }
            slot.cancel_if_incomplete();
        }
    }

    #[cold]
    #[inline(never)]
    fn mark_done(&mut self) {
        if let Some(slot) = self.slot.take() {
            if self.state == LeaseState::AwaitingStart {
                slot.close_receiver();
                self.state = LeaseState::Active;
            }
            slot.mark_done();
        }
    }
}

impl Drop for RequestLease {
    #[inline]
    fn drop(&mut self) {
        if self.slot.is_some() {
            self.cancel_slot();
        }
        self.phase.leave_request();
        self.inflight.dec();
    }
}

/// Per-request shard lifecycle guard owned during admission and PHP dispatch, then converted into `TrackBody<B>`.
pub(crate) struct RequestScope {
    lease: RequestLease,
}

impl RequestScope {
    #[inline]
    pub(crate) fn new(phase: Rc<ConnPhase>, inflight: Rc<LocalInflight>) -> Self {
        phase.enter_request();
        inflight.inc();
        Self {
            lease: RequestLease {
                phase,
                inflight,
                slot: None,
                state: LeaseState::Active,
            },
        }
    }

    #[inline]
    pub(crate) fn attach_slot(&mut self, slot: Arc<ResponseSlot>) {
        self.lease.slot = Some(slot);
        self.lease.state = LeaseState::AwaitingStart;
    }

    /// Awaits the initial `Start` message directly on the attached `ResponseSlot` without cloning `Arc`.
    pub(crate) async fn wait_start(&mut self) -> Result<Start, ()> {
        std::future::poll_fn(|cx| {
            let slot = self
                .lease
                .slot
                .as_ref()
                .expect("slot attached before wait_start");
            match slot.poll_recv(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(out) => {
                    self.lease.state = LeaseState::Active;
                    Poll::Ready(out)
                }
            }
        })
        .await
    }

    /// Wraps a normal HTTP response so `ConnPhase` arms completion once the full body is ready and cancels on abort or drop.
    #[inline]
    pub(crate) fn track(
        mut self,
        resp: Response<ResponseBody>,
    ) -> Response<TrackBody<ResponseBody>> {
        if resp.body().is_complete() {
            self.lease.arm_complete();
        }
        let lease = self.lease;
        resp.map(|body| TrackBody { body, lease })
    }

    /// Returns a host refusal response, marking any attached PHP slot done immediately without arming `ConnPhase`.
    pub(crate) fn refuse(mut self, status: StatusCode) -> Response<TrackBody<ResponseBody>> {
        self.lease.mark_done();
        let lease = self.lease;
        refusal(status).map(|body| TrackBody { body, lease })
    }
}

/// Shard-level `http_body::Body` adapter that tracks request inflight count, `ConnPhase` arming, and abort cancellation.
pub(crate) struct TrackBody<B> {
    body: B,
    lease: RequestLease,
}

type FramePoll<B> = Poll<Option<Result<Frame<<B as Body>::Data>, <B as Body>::Error>>>;

impl<B: Body + Unpin> TrackBody<B> {
    #[cold]
    #[inline(never)]
    fn on_stream_poll(&mut self, poll: &FramePoll<B>) {
        match poll {
            Poll::Pending => {}
            Poll::Ready(Some(Ok(_))) => {
                if self.lease.state != LeaseState::Aborted && self.body.is_end_stream() {
                    self.lease.arm_complete();
                }
            }
            Poll::Ready(Some(Err(_))) => {
                self.lease.state = LeaseState::Aborted;
                self.lease.cancel_slot();
            }
            Poll::Ready(None) => {
                if self.lease.state != LeaseState::Aborted {
                    self.lease.arm_complete();
                }
            }
        }
    }
}

impl<B: Body + Unpin> Body for TrackBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }

    #[inline]
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.body).poll_frame(cx);
        if this.lease.has_slot() {
            this.on_stream_poll(&poll);
        }
        poll
    }
}

/// `hyper::rt::{Read, Write}` wrapper that allocates and arms timers lazily only on `Poll::Pending`.
pub(crate) struct ShardIo<S> {
    io: TokioIo<S>,
    phase: Rc<ConnPhase>,
    keepalive_timeout: Duration,
    /// Coarsening slack that avoids per-request `Sleep::reset()` wheel churn on busy keep-alive connections.
    keepalive_slack: Duration,
    write_timeout: Duration,
    read_sleep: Option<Pin<Box<Sleep>>>,
    read_armed_epoch: Option<u64>,
    read_idle_since: Option<Instant>,
    write_sleep: Option<Pin<Box<Sleep>>>,
    write_armed: bool,
}

impl<S> ShardIo<S> {
    pub(crate) fn new(
        raw: S,
        phase: Rc<ConnPhase>,
        keepalive_timeout: Duration,
        write_timeout: Duration,
    ) -> Self {
        let keepalive_slack = (keepalive_timeout / 4).min(Duration::from_millis(250));
        Self {
            io: TokioIo::new(raw),
            phase,
            keepalive_timeout,
            keepalive_slack,
            write_timeout,
            read_sleep: None,
            read_armed_epoch: None,
            read_idle_since: None,
            write_sleep: None,
            write_armed: false,
        }
    }

    fn poll_write_stall(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.write_armed {
            let deadline = Instant::now() + self.write_timeout;
            match &mut self.write_sleep {
                Some(s) => s.as_mut().reset(deadline),
                None => self.write_sleep = Some(Box::pin(tokio::time::sleep_until(deadline))),
            }
            self.write_armed = true;
        }
        if let Some(sleep) = &mut self.write_sleep
            && sleep.as_mut().poll(cx).is_ready()
        {
            self.write_armed = false;
            self.phase.cancel_pending();
            tracing::warn!(target: "http", "response write timed out");
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "response write timed out",
            )));
        }
        Poll::Pending
    }
}

impl<S: AsyncRead + Unpin> Read for ShardIo<S> {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.io).poll_read(cx, buf) {
            Poll::Ready(res) => Poll::Ready(res),
            Poll::Pending => {
                let (phase, epoch) = this.phase.current();
                if phase != IoPhase::ReadHead {
                    return Poll::Pending;
                }
                if this.read_armed_epoch != Some(epoch) {
                    let now = Instant::now();
                    let target = now + this.keepalive_timeout;
                    this.read_idle_since = Some(now);
                    match &mut this.read_sleep {
                        Some(s) => {
                            if s.deadline() + this.keepalive_slack < target {
                                s.as_mut().reset(target);
                            }
                        }
                        None => {
                            this.read_sleep = Some(Box::pin(tokio::time::sleep_until(target)));
                        }
                    }
                    this.read_armed_epoch = Some(epoch);
                }
                if let Some(sleep) = &mut this.read_sleep
                    && sleep.as_mut().poll(cx).is_ready()
                {
                    if let Some(since) = this.read_idle_since {
                        let exact = since + this.keepalive_timeout;
                        if Instant::now() < exact {
                            sleep.as_mut().reset(exact);
                            if sleep.as_mut().poll(cx).is_pending() {
                                return Poll::Pending;
                            }
                        }
                    }
                    this.read_armed_epoch = None;
                    this.read_idle_since = None;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "keepalive timeout",
                    )));
                }
                Poll::Pending
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> Write for ShardIo<S> {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.io).poll_write(cx, buf) {
            Poll::Ready(res) => {
                this.write_armed = false;
                if res.is_err() {
                    this.phase.cancel_pending();
                }
                Poll::Ready(res)
            }
            Poll::Pending => this.poll_write_stall(cx).map(|r| r.map(|()| 0)),
        }
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.io).poll_flush(cx) {
            Poll::Ready(res) => {
                this.write_armed = false;
                if res.is_ok() {
                    this.phase.on_flushed();
                } else {
                    this.phase.cancel_pending();
                }
                Poll::Ready(res)
            }
            Poll::Pending => this.poll_write_stall(cx),
        }
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.io).poll_write_vectored(cx, bufs) {
            Poll::Ready(res) => {
                this.write_armed = false;
                if res.is_err() {
                    this.phase.cancel_pending();
                }
                Poll::Ready(res)
            }
            Poll::Pending => this.poll_write_stall(cx).map(|r| r.map(|()| 0)),
        }
    }
}
