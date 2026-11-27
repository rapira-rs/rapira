//! LocalSet-friendly `hyper::rt::{Read, Write}` transport with lazy single-timer idle and write timeouts.

use std::cell::Cell;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::{Instant, Sleep};

use crate::responder::ResponseSlot;

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

/// `hyper::rt::{Read, Write}` wrapper that allocates and arms timers lazily only on `Poll::Pending`.
pub(crate) struct ShardIo {
    io: TokioIo<RawIo>,
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

impl ShardIo {
    pub(crate) fn new(
        raw: RawIo,
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

impl Read for ShardIo {
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

impl Write for ShardIo {
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
        true
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
