use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use rapira_sapi::Frame;
use tokio::sync::mpsc::Receiver;
use tokio::sync::watch;

use crate::handler::InflightReqCount;
use crate::middleware::BoxError;

#[derive(Clone, Copy, Default)]
pub(crate) struct ConnectionState {
    pub(crate) closed: bool,
    pub(crate) flushes: u64,
}

pub(crate) struct ReplyBody {
    reply: Option<Receiver<Frame>>,
    declared_cl: Option<u64>,
    sent: u64,
    staged: Option<Frame>,
    file: Option<FilePump>,
    err_armed: bool,
    closed: watch::Receiver<ConnectionState>,
    guard: Arc<InflightReqCount>,
}

type FileRead = tokio::task::JoinHandle<(std::fs::File, std::io::Result<Vec<u8>>)>;

struct FilePump {
    join: FileRead,
    offset: u64,
    len: u64,
    done: u64,
}

fn read_slice(file: std::fs::File, off: u64, want: usize) -> FileRead {
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; want];
        let res = file.read_at(&mut buf, off).map(|n| {
            buf.truncate(n);
            buf
        });
        (file, res)
    })
}

impl ReplyBody {
    pub(crate) fn new(
        reply: Receiver<Frame>,
        declared_cl: Option<u64>,
        guard: Arc<InflightReqCount>,
        staged: Option<Frame>,
        closed: watch::Receiver<ConnectionState>,
    ) -> Self {
        Self {
            reply: Some(reply),
            declared_cl,
            sent: 0,
            staged,
            file: None,
            err_armed: false,
            closed,
            guard,
        }
    }

    fn terminal_error(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, BoxError>>> {
        self.reply = None;
        self.file = None;
        self.err_armed = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl http_body::Body for ReplyBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        loop {
            if this.err_armed {
                return Poll::Ready(Some(Err("php response truncated".into())));
            }
            if let Some(fp) = &mut this.file {
                let (file, res) = match std::task::ready!(Pin::new(&mut fp.join).poll(cx)) {
                    Ok(v) => v,
                    Err(e) => {
                        if e.is_cancelled() {
                            tracing::debug!(target: "http", "sendfile read cancelled at shutdown");
                        } else {
                            tracing::error!(target: "http", "sendfile read task failed: {e}");
                        }
                        return this.terminal_error(cx);
                    }
                };
                let buf = match res {
                    Ok(buf) => buf,
                    Err(e) => {
                        tracing::error!(
                            target: "http",
                            "sendfile read failed with {} byte(s) left: {e}",
                            fp.len - fp.done
                        );
                        return this.terminal_error(cx);
                    }
                };
                if buf.is_empty() {
                    tracing::warn!(
                        target: "http",
                        "sendfile slice ended {} byte(s) short: the file shrank mid-send",
                        fp.len - fp.done
                    );
                    return this.terminal_error(cx);
                }
                fp.done += buf.len() as u64;
                this.sent += buf.len() as u64;
                if fp.done < fp.len {
                    let want = std::cmp::min(64 * 1024, fp.len - fp.done) as usize;
                    fp.join = read_slice(file, fp.offset + fp.done, want);
                } else {
                    this.file = None;
                }
                return Poll::Ready(Some(Ok(http_body::Frame::data(buf.into()))));
            }
            let ev: Option<Frame> = if let Some(ev) = this.staged.take() {
                Some(ev)
            } else {
                let Some(reply) = this.reply.as_mut() else {
                    return Poll::Ready(None);
                };
                std::task::ready!(reply.poll_recv(cx))
            };
            match ev {
                None => {
                    tracing::warn!(
                        target: "http",
                        "php worker died mid-body; response truncated after {} byte(s)",
                        this.sent
                    );
                    return this.terminal_error(cx);
                }
                Some(Frame::Chunk(b)) => {
                    this.sent += b.len() as u64;
                    return Poll::Ready(Some(Ok(http_body::Frame::data(b))));
                }
                Some(Frame::File { file, offset, len }) => {
                    let want = std::cmp::min(64 * 1024, len) as usize;
                    this.file = Some(FilePump {
                        join: read_slice(file, offset, want),
                        offset,
                        len,
                        done: 0,
                    });
                }
                // The producer latches the head, so neither event can follow it.
                Some(Frame::Interim(_) | Frame::Head { .. }) => {}
                Some(Frame::End { truncated, .. }) => {
                    if truncated {
                        tracing::debug!(target: "http", "php ended the reply as truncated");
                        return this.terminal_error(cx);
                    }
                    if let Some(cl) = this.declared_cl.filter(|&cl| this.sent < cl) {
                        tracing::warn!(
                            target: "http",
                            "php declared content-length {cl} but ended after {} byte(s); the response was cut short",
                            this.sent
                        );
                        return this.terminal_error(cx);
                    }
                    this.reply = None;
                    return Poll::Ready(None);
                }
            }
        }
    }
}

impl Drop for ReplyBody {
    fn drop(&mut self) {
        // A length-delimited HTTP body can finish before PHP sends End: https://www.rfc-editor.org/rfc/rfc9112#section-6.3
        if self.declared_cl == Some(self.sent)
            && !matches!(self.staged, Some(Frame::End { .. }))
            && self.guard.end_flush.get().is_some()
            && let Some(reply) = self.reply.take()
        {
            spawn_drain(reply, self.closed.clone(), Arc::clone(&self.guard));
        }
    }
}

/// Consumes the reply to End. A connection close cancels the reply unless a flush past the guard's watermark has written the last response byte to the socket.
/// After that flush the drain holds the reply until PHP sends End, so a delivered response never reports cancellation to PHP.
pub(crate) fn spawn_drain(
    mut reply: Receiver<Frame>,
    mut closed: watch::Receiver<ConnectionState>,
    guard: Arc<InflightReqCount>,
) {
    // A buffered reply queues End behind the head or the last chunk, so consume it here.
    let mut cx = Context::from_waker(std::task::Waker::noop());
    if let Poll::Ready(Some(Frame::End { .. }) | None) = reply.poll_recv(&mut cx) {
        return;
    }
    tokio::spawn(async move {
        let flushed = |s: &ConnectionState| guard.end_flush.get().is_some_and(|&f| s.flushes > f);
        tokio::select! {
            biased;
            state = closed.wait_for(|s| s.closed || flushed(s)) => {
                if !state.is_ok_and(|s| flushed(&s)) {
                    return;
                }
            }
            () = drain(&mut reply) => return,
        }
        drain(&mut reply).await;
    });
}

async fn drain(reply: &mut Receiver<Frame>) {
    while let Some(ev) = reply.recv().await {
        if matches!(ev, Frame::End { .. }) {
            break;
        }
    }
}

pub(crate) struct TimedIo<T> {
    io: T,
    timeout: Duration,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    state: watch::Sender<ConnectionState>,
}

impl<T> TimedIo<T> {
    pub(crate) fn new(io: T, timeout: Duration, state: watch::Sender<ConnectionState>) -> Self {
        Self {
            io,
            timeout,
            deadline: None,
            state,
        }
    }

    fn stalled(&mut self, cx: &mut Context<'_>) -> Option<std::io::Error> {
        let deadline = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.timeout)));
        if deadline.as_mut().poll(cx).is_ready() {
            tracing::debug!(target: "http", "response write timed out; closing the connection");
            return Some(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "response write timed out",
            ));
        }
        None
    }

    fn gate<R>(
        &mut self,
        cx: &mut Context<'_>,
        poll: Poll<std::io::Result<R>>,
    ) -> Poll<std::io::Result<R>> {
        match poll {
            Poll::Ready(r) => {
                self.deadline = None;
                Poll::Ready(r)
            }
            Poll::Pending => match self.stalled(cx) {
                Some(e) => Poll::Ready(Err(e)),
                None => Poll::Pending,
            },
        }
    }
}

impl<T: hyper::rt::Read + Unpin> hyper::rt::Read for TimedIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: hyper::rt::Write + Unpin> hyper::rt::Write for TimedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_write(cx, buf);
        this.gate(cx, poll)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_flush(cx);
        let poll = this.gate(cx, poll);
        if matches!(poll, Poll::Ready(Ok(()))) {
            // The drain reads the count when the close wakes it, so a flush sends no notification.
            this.state.send_if_modified(|s| {
                s.flushes += 1;
                false
            });
        }
        poll
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.gate(cx, poll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Weak};
    use tokio::sync::mpsc;

    /// A reply that yields `events`; it stays open while the caller holds the sender.
    fn open_reply(events: Vec<Frame>) -> (Receiver<Frame>, mpsc::Sender<Frame>) {
        let (tx, rx) = mpsc::channel(4);
        for ev in events {
            tx.try_send(ev).unwrap();
        }
        (rx, tx)
    }

    /// A reply that yields `events` and then closes.
    fn reply(events: Vec<Frame>) -> Receiver<Frame> {
        open_reply(events).0
    }

    fn end(truncated: bool) -> Frame {
        Frame::End {
            trailers: http::HeaderMap::new(),
            truncated,
        }
    }

    fn chunk(s: &str) -> Frame {
        Frame::Chunk(Bytes::copy_from_slice(s.as_bytes()))
    }

    fn guard() -> Arc<InflightReqCount> {
        Arc::new(InflightReqCount::init(&Arc::new(AtomicUsize::new(0))))
    }

    fn body(events: Vec<Frame>, declared_cl: Option<u64>) -> ReplyBody {
        ReplyBody::new(
            reply(events),
            declared_cl,
            guard(),
            None,
            watch::channel(ConnectionState::default()).1,
        )
    }

    /// The pre-fetched first event streams ahead of everything the source still holds.
    #[tokio::test]
    async fn staged_event_streams_before_the_source() {
        let mut b = ReplyBody::new(
            reply(vec![chunk("second"), end(false)]),
            None,
            guard(),
            Some(chunk("first")),
            watch::channel(ConnectionState::default()).1,
        );
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "first");
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "second");
        assert!(data(&mut b).await.is_none());
    }

    async fn data(body: &mut ReplyBody) -> Option<Result<Bytes, String>> {
        body.frame().await.map(|r| {
            r.map(|f| f.into_data().expect("data frame"))
                .map_err(|e| e.to_string())
        })
    }

    #[tokio::test]
    async fn frames_stream_in_order_and_end_cleanly() {
        let mut b = body(vec![chunk("ab"), chunk("c"), end(false)], Some(3));
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "ab");
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "c");
        assert!(data(&mut b).await.is_none());
    }

    #[tokio::test]
    async fn truncated_end_becomes_a_body_error() {
        let mut b = body(vec![chunk("x"), end(true)], None);
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "x");
        let err = data(&mut b).await.unwrap().unwrap_err();
        assert!(err.contains("truncated"), "{err}");
    }

    #[tokio::test]
    async fn stream_death_without_end_becomes_a_body_error() {
        let mut b = body(vec![chunk("x")], None);
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "x");
        assert!(data(&mut b).await.unwrap().is_err());
    }

    #[tokio::test]
    async fn short_body_against_declared_length_becomes_a_body_error() {
        let mut b = body(vec![chunk("abc"), end(false)], Some(10));
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "abc");
        assert!(data(&mut b).await.unwrap().is_err());
    }

    /// The terminal error must let hyper flush first: one Pending pass with a wake, then the error.
    #[test]
    fn error_is_gated_behind_one_flush_pass() {
        use http_body::Body as _;
        use std::task::{Wake, Waker};
        struct Flag(AtomicBool);
        impl Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }

        let mut b = body(vec![end(true)], None);
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut cx = Context::from_waker(&waker);
        let first = Pin::new(&mut b).poll_frame(&mut cx);
        assert!(
            matches!(first, Poll::Pending),
            "first poll must let hyper flush"
        );
        assert!(flag.0.load(Ordering::Acquire), "the gate must self-wake");
        let second = Pin::new(&mut b).poll_frame(&mut cx);
        assert!(matches!(second, Poll::Ready(Some(Err(_)))));
    }

    /// Dropping the body drops the reply synchronously: the client-gone signal for PHP.
    #[test]
    fn dropping_the_body_cancels_php() {
        let (reply, events) = open_reply(vec![chunk("head-flushed")]);
        let b = ReplyBody::new(
            reply,
            None,
            guard(),
            None,
            watch::channel(ConnectionState::default()).1,
        );
        drop(b);
        assert!(events.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_write_times_out_and_errors_the_connection() {
        struct Stuck;
        impl hyper::rt::Write for Stuck {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Pending
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
        }
        let mut io = TimedIo::new(
            Stuck,
            Duration::from_secs(30),
            watch::channel(ConnectionState::default()).0,
        );
        // The paused clock auto-advances once every task is idle, firing the deadline.
        let err =
            std::future::poll_fn(|cx| hyper::rt::Write::poll_write(Pin::new(&mut io), cx, b"x"))
                .await
                .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    struct Drain {
        inflight: Arc<AtomicUsize>,
        /// Weak: the drain task stays the only owner of the request count.
        guard: Weak<InflightReqCount>,
        events: mpsc::Sender<Frame>,
        state: watch::Sender<ConnectionState>,
    }

    impl Drain {
        /// Records the watermark after the drain started, as `respond` does for a bodiless reply.
        fn mark(&self, flush: u64) {
            let guard = self.guard.upgrade().expect("the drain must hold the guard");
            guard.end_flush.set(flush).unwrap();
        }
    }

    /// A drain parked on an empty reply after two chunks, with the request still counted.
    async fn parked_drain() -> Drain {
        let inflight = Arc::new(AtomicUsize::new(0));
        let guard = Arc::new(InflightReqCount::init(&inflight));
        let weak = Arc::downgrade(&guard);
        let (reply, events) = open_reply(vec![chunk("queued")]);
        let (state, state_rx) = watch::channel(ConnectionState::default());
        spawn_drain(reply, state_rx, guard);
        assert!(
            !events.is_closed(),
            "a chunk at the head must hand the reply to the drain task"
        );
        events.send(chunk("discarded")).await.unwrap();
        // The drain task takes the chunk and parks on the empty reply.
        tokio::time::timeout(Duration::from_secs(5), async {
            while events.capacity() < events.max_capacity() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drain must reach a pending state after the chunk");
        assert!(
            !events.is_closed(),
            "drain must retain the reply while it is pending"
        );
        assert_eq!(inflight.load(Ordering::Acquire), 1);
        Drain {
            inflight,
            guard: weak,
            events,
            state,
        }
    }

    /// End alone ends the drain and releases the request count.
    #[tokio::test]
    async fn bodiless_drain_consumes_to_end_without_cancelling() {
        let d = parked_drain().await;
        d.events.send(end(false)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), d.events.closed())
            .await
            .expect("drain must drop the reply after End");
        assert_eq!(d.inflight.load(Ordering::Acquire), 0);
    }

    /// Without a watermark, a client disconnect cancels the drain.
    #[tokio::test]
    async fn drain_cancels_when_the_connection_closes() {
        let d = parked_drain().await;
        d.state.send_modify(|s| s.closed = true);
        tokio::time::timeout(Duration::from_secs(5), d.events.closed())
            .await
            .expect("drain must drop the reply when the connection closes");
        assert_eq!(d.inflight.load(Ordering::Acquire), 0);
    }

    /// A flush past the watermark wrote the last byte to the socket: a later close must not cancel the reply.
    #[tokio::test(start_paused = true)]
    async fn flushed_drain_survives_a_later_close() {
        let d = parked_drain().await;
        d.mark(0);
        d.state.send_modify(|s| s.flushes = 1);
        d.state.send_modify(|s| s.closed = true);
        // The paused clock auto-advances once every task is idle, so the timeout proves the drain kept the reply.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), d.events.closed())
                .await
                .is_err(),
            "the reply must outlive the connection"
        );
        assert_eq!(d.inflight.load(Ordering::Acquire), 1);
        d.events.send(end(false)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), d.events.closed())
            .await
            .expect("drain must drop the reply after End");
        assert_eq!(d.inflight.load(Ordering::Acquire), 0);
    }

    /// A flush at the watermark is not past it: a close then cancels the reply.
    #[tokio::test]
    async fn drain_cancels_when_the_close_beats_the_flush() {
        let d = parked_drain().await;
        d.mark(1);
        d.state.send_modify(|s| s.flushes = 1);
        d.state.send_modify(|s| s.closed = true);
        tokio::time::timeout(Duration::from_secs(5), d.events.closed())
            .await
            .expect("drain must drop the reply when the connection closes before the flush");
        assert_eq!(d.inflight.load(Ordering::Acquire), 0);
    }

    /// One chunk and no End, the shape of an unfinalized PHP exchange; the sender closes when the reply is dropped.
    fn parked_source() -> (Receiver<Frame>, mpsc::Sender<Frame>) {
        open_reply(vec![chunk("abc")])
    }

    /// A completed length-delimited body hands its reply to the drain once the watermark is set.
    #[tokio::test]
    async fn completed_body_keeps_the_reply_for_the_drain() {
        let (reply, events) = parked_source();
        let guard = guard();
        guard.end_flush.set(0).unwrap();
        let (state, state_rx) = watch::channel(ConnectionState::default());
        let mut b = ReplyBody::new(reply, Some(3), guard, None, state_rx);
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "abc");
        drop(b);
        assert!(!events.is_closed(), "the drain must hold the reply");
        state.send_modify(|s| s.closed = true);
        tokio::time::timeout(Duration::from_secs(5), events.closed())
            .await
            .expect("an unflushed drain must cancel on close");
    }

    /// The buffered path queues End behind the last chunk: drop consumes it without a drain task.
    #[tokio::test]
    async fn queued_end_is_consumed_at_drop_without_a_task() {
        let guard = guard();
        guard.end_flush.set(0).unwrap();
        let (reply, events) = open_reply(vec![chunk("hello"), end(false)]);
        let mut b = ReplyBody::new(
            reply,
            Some(5),
            guard,
            None,
            watch::channel(ConnectionState::default()).1,
        );
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "hello");
        let metrics = tokio::runtime::Handle::current().metrics();
        let tasks = metrics.num_alive_tasks();
        drop(b);
        assert!(
            events.is_closed(),
            "drop must consume the queued End in place"
        );
        assert_eq!(
            metrics.num_alive_tasks(),
            tasks,
            "a queued End must not cost a drain task"
        );
    }

    /// A bodiless reply with End already queued behind the head is consumed in place.
    #[tokio::test]
    async fn queued_end_is_consumed_without_a_drain_task() {
        let (reply, events) = open_reply(vec![end(false)]);
        let metrics = tokio::runtime::Handle::current().metrics();
        let tasks = metrics.num_alive_tasks();
        spawn_drain(reply, watch::channel(ConnectionState::default()).1, guard());
        assert!(events.is_closed(), "the reply must drop at once");
        assert_eq!(
            metrics.num_alive_tasks(),
            tasks,
            "a queued End must not cost a drain task"
        );
    }

    /// A reply that has not sent End yet is still pending at drop, so the drain task takes it.
    #[tokio::test(start_paused = true)]
    async fn pending_reply_goes_to_the_drain_task() {
        let guard = guard();
        guard.end_flush.set(0).unwrap();
        let (reply, events) = open_reply(vec![chunk("abc")]);
        let (_state, state_rx) = watch::channel(ConnectionState::default());
        let mut b = ReplyBody::new(reply, Some(3), guard, None, state_rx);
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "abc");
        drop(b);
        // The paused clock auto-advances once every task is idle, so the timeout proves the drain kept the reply.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), events.closed())
                .await
                .is_err(),
            "the drain task must hold the reply until End arrives"
        );
        events.send(end(false)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), events.closed())
            .await
            .expect("the drain task must consume End");
    }

    /// Without the watermark the bytes never reached hyper: dropping the body cancels PHP at once.
    #[tokio::test]
    async fn completed_body_without_a_watermark_cancels_php() {
        let (reply, events) = parked_source();
        let mut b = ReplyBody::new(
            reply,
            Some(3),
            guard(),
            None,
            watch::channel(ConnectionState::default()).1,
        );
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "abc");
        drop(b);
        assert!(events.is_closed());
    }
}
