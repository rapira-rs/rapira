use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use extension_api::{BoxError, Reply, ReplyEvent};
use tokio::sync::watch;

use crate::handler::InflightReqCount;

#[derive(Clone, Copy, Default)]
pub(crate) struct ConnectionState {
    pub(crate) closed: bool,
    pub(crate) flushes: u64,
}

pub(crate) struct ReplyBody {
    reply: Option<Reply>,
    declared_cl: Option<u64>,
    sent: u64,
    staged: Option<ReplyEvent>,
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
        reply: Reply,
        declared_cl: Option<u64>,
        guard: Arc<InflightReqCount>,
        staged: Option<ReplyEvent>,
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
            let ev: Option<ReplyEvent> = if let Some(ev) = this.staged.take() {
                Some(ev)
            } else {
                let Some(reply) = this.reply.as_mut() else {
                    return Poll::Ready(None);
                };
                std::task::ready!(reply.poll_next(cx))
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
                Some(ReplyEvent::Chunk(b)) => {
                    this.sent += b.len() as u64;
                    return Poll::Ready(Some(Ok(http_body::Frame::data(b))));
                }
                Some(ReplyEvent::File { file, offset, len }) => {
                    let want = std::cmp::min(64 * 1024, len) as usize;
                    this.file = Some(FilePump {
                        join: read_slice(file, offset, want),
                        offset,
                        len,
                        done: 0,
                    });
                }
                // The producer latches the head, so neither event can follow it.
                Some(ReplyEvent::Interim { .. } | ReplyEvent::Head { .. }) => {}
                Some(ReplyEvent::End { truncated, .. }) => {
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

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::default()
    }
}

impl Drop for ReplyBody {
    fn drop(&mut self) {
        // A length-delimited HTTP body can finish before PHP sends End: https://www.rfc-editor.org/rfc/rfc9112.html#section-6.3
        if self.declared_cl == Some(self.sent)
            && !matches!(self.staged, Some(ReplyEvent::End { .. }))
            && self.guard.end_flush.get().is_some()
            && let Some(reply) = self.reply.take()
        {
            spawn_drain(reply, self.closed.clone(), Arc::clone(&self.guard));
        }
    }
}

/// Consumes the reply to End. A connection close cancels the reply until a flush past the guard's watermark proves delivery.
pub(crate) fn spawn_drain(
    mut reply: Reply,
    mut closed: watch::Receiver<ConnectionState>,
    guard: Arc<InflightReqCount>,
) {
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

async fn drain(reply: &mut Reply) {
    while let Some(ev) = reply.next().await {
        if matches!(ev, ReplyEvent::End { .. }) {
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
            this.state.send_modify(|s| s.flushes += 1);
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
    use extension_api::ReplySource;
    use http_body_util::BodyExt;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Weak};

    struct Script {
        events: VecDeque<ReplyEvent>,
        dropped: Option<Arc<AtomicBool>>,
        /// Park forever once the events run out, like a worker that never finishes.
        hang: bool,
    }

    impl ReplySource for Script {
        fn poll_next(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ReplyEvent>> {
            match self.events.pop_front() {
                Some(ev) => Poll::Ready(Some(ev)),
                None if self.hang => Poll::Pending,
                None => Poll::Ready(None),
            }
        }
    }

    impl Drop for Script {
        fn drop(&mut self) {
            if let Some(flag) = &self.dropped {
                flag.store(true, Ordering::Release);
            }
        }
    }

    struct DrainSource {
        events: tokio::sync::mpsc::UnboundedReceiver<ReplyEvent>,
        pending: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl ReplySource for DrainSource {
        fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<ReplyEvent>> {
            let poll = self.events.poll_recv(cx);
            if poll.is_pending()
                && let Some(pending) = self.pending.take()
            {
                let _ = pending.send(());
            }
            poll
        }
    }

    fn drain_reply() -> (
        Reply,
        tokio::sync::mpsc::UnboundedSender<ReplyEvent>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (pending_tx, pending_rx) = tokio::sync::oneshot::channel();
        let source = DrainSource {
            events: event_rx,
            pending: Some(pending_tx),
        };
        (Reply::new(Box::new(source)), event_tx, pending_rx)
    }

    fn reply(events: Vec<ReplyEvent>) -> Reply {
        Reply::new(Box::new(Script {
            events: events.into(),
            dropped: None,
            hang: false,
        }))
    }

    fn end(truncated: bool) -> ReplyEvent {
        ReplyEvent::End {
            trailers: Vec::new(),
            truncated,
        }
    }

    fn chunk(s: &str) -> ReplyEvent {
        ReplyEvent::Chunk(Bytes::copy_from_slice(s.as_bytes()))
    }

    fn guard() -> Arc<InflightReqCount> {
        Arc::new(InflightReqCount::init(&Arc::new(AtomicUsize::new(0))))
    }

    fn body(events: Vec<ReplyEvent>, declared_cl: Option<u64>) -> ReplyBody {
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
        let dropped = Arc::new(AtomicBool::new(false));
        let source = Script {
            events: vec![chunk("head-flushed")].into(),
            dropped: Some(Arc::clone(&dropped)),
            hang: true,
        };
        let b = ReplyBody::new(
            Reply::new(Box::new(source)),
            None,
            guard(),
            None,
            watch::channel(ConnectionState::default()).1,
        );
        drop(b);
        assert!(dropped.load(Ordering::Acquire));
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
        events: tokio::sync::mpsc::UnboundedSender<ReplyEvent>,
        state: watch::Sender<ConnectionState>,
    }

    impl Drain {
        /// Records the watermark after the drain started, as `RapiraService::call` does for a bodiless reply.
        fn mark(&self, flush: u64) {
            let guard = self.guard.upgrade().expect("the drain must hold the guard");
            guard.end_flush.set(flush).unwrap();
        }
    }

    /// A drain parked on an empty reply after one chunk, with the request still counted.
    async fn parked_drain() -> Drain {
        let inflight = Arc::new(AtomicUsize::new(0));
        let guard = Arc::new(InflightReqCount::init(&inflight));
        let weak = Arc::downgrade(&guard);
        let (reply, events, pending) = drain_reply();
        events.send(chunk("discarded")).unwrap();
        let (state, state_rx) = watch::channel(ConnectionState::default());
        spawn_drain(reply, state_rx, guard);
        tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("drain must reach a pending state after the chunk")
            .expect("drain must retain the reply while it is pending");
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
        d.events.send(end(false)).unwrap();
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

    /// A flush past the watermark proves delivery: a later close must not cancel the reply.
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
        d.events.send(end(false)).unwrap();
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

    /// One chunk, then parked like an unfinalized PHP exchange; `dropped` turns true when the reply is dropped.
    fn parked_source(dropped: &Arc<AtomicBool>) -> Reply {
        Reply::new(Box::new(Script {
            events: vec![chunk("abc")].into(),
            dropped: Some(Arc::clone(dropped)),
            hang: true,
        }))
    }

    /// A completed length-delimited body hands its reply to the drain once the watermark is set.
    #[tokio::test]
    async fn completed_body_keeps_the_reply_for_the_drain() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = guard();
        guard.end_flush.set(0).unwrap();
        let (state, state_rx) = watch::channel(ConnectionState::default());
        let mut b = ReplyBody::new(parked_source(&dropped), Some(3), guard, None, state_rx);
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "abc");
        drop(b);
        assert!(
            !dropped.load(Ordering::Acquire),
            "the drain must hold the reply"
        );
        state.send_modify(|s| s.closed = true);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("an unflushed drain must cancel on close");
    }

    /// Without the watermark the bytes never reached hyper: dropping the body cancels PHP at once.
    #[tokio::test]
    async fn completed_body_without_a_watermark_cancels_php() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut b = ReplyBody::new(
            parked_source(&dropped),
            Some(3),
            guard(),
            None,
            watch::channel(ConnectionState::default()).1,
        );
        assert_eq!(data(&mut b).await.unwrap().unwrap(), "abc");
        drop(b);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn file_event_streams_the_slice_in_chunks() {
        use std::io::Write;
        let mut f = tempfile::tempfile().unwrap();
        let payload = vec![7u8; 100 * 1024];
        f.write_all(&payload).unwrap();
        let mut b = body(
            vec![
                ReplyEvent::File {
                    file: f,
                    offset: 1024,
                    len: 80 * 1024,
                },
                end(false),
            ],
            None,
        );
        let mut got: Vec<u8> = Vec::new();
        while let Some(r) = data(&mut b).await {
            got.extend_from_slice(&r.unwrap());
        }
        assert_eq!(got.len(), 80 * 1024);
        assert!(got.iter().all(|&x| x == 7));
    }

    /// A file that shrank below the promised slice aborts instead of faking a clean end.
    #[tokio::test]
    async fn shrunken_file_becomes_a_body_error() {
        use std::io::Write;
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&vec![7u8; 64 * 1024]).unwrap();
        let mut b = body(
            vec![
                ReplyEvent::File {
                    file: f,
                    offset: 0,
                    len: 90 * 1024,
                },
                end(false),
            ],
            None,
        );
        let mut got = 0usize;
        let err = loop {
            match data(&mut b).await.unwrap() {
                Ok(bytes) => got += bytes.len(),
                Err(e) => break e,
            }
        };
        assert_eq!(got, 64 * 1024);
        assert!(err.contains("truncated"), "{err}");
    }
}
