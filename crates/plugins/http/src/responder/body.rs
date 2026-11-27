//! Concrete `http_body::Body` and unified streaming, sendfile, encoder, and abort state machine.

use std::fmt;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::unix::fs::FileExt;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use rapira_http_middleware::{BodyEncoder, BodyStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::slot::{Chunk, PooledBuf, take_pooled_buf};

const SENDFILE_SLICE: usize = 256 << 10;

/// Error returned from `ResponseBody::poll_frame` so `hyper` drops the connection without writing a chunk terminator.
#[derive(Debug)]
pub(crate) struct StreamAbort;

impl fmt::Display for StreamAbort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("response stream aborted")
    }
}

impl std::error::Error for StreamAbort {}

/// Runs `encoders` inner-to-outer on `input`. When `finish` is true, finalizes each encoder; otherwise flushes after writing.
pub(super) fn run_encoders(
    encoders: &mut [Box<dyn BodyEncoder>],
    input: &[u8],
    finish: bool,
) -> io::Result<Bytes> {
    let apply = |enc: &mut dyn BodyEncoder, data: &[u8], out: &mut Vec<u8>| -> io::Result<()> {
        if finish {
            if !data.is_empty() {
                enc.write(data, out)?;
            }
            enc.finish(out)
        } else {
            enc.write(data, out)?;
            enc.flush(out)
        }
    };
    if encoders.len() == 1 {
        let mut out = Vec::new();
        apply(encoders[0].as_mut(), input, &mut out)?;
        return Ok(Bytes::from(out));
    }
    let mut cur = input.to_vec();
    let mut next = Vec::new();
    for enc in encoders.iter_mut() {
        next.clear();
        apply(enc.as_mut(), &cur, &mut next)?;
        std::mem::swap(&mut cur, &mut next);
    }
    Ok(Bytes::from(cur))
}

type FileRead = (File, Vec<u8>, io::Result<usize>);

/// Reads a `sendFile()` slice asynchronously via `spawn_blocking` using pooled buffers.
struct ActiveFile {
    file: Option<File>,
    offset: u64,
    remaining: u64,
    in_flight: Option<JoinHandle<FileRead>>,
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

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, ()>>> {
        loop {
            if let Some(handle) = &mut self.in_flight {
                let out = match Pin::new(handle).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(out) => {
                        self.in_flight = None;
                        out
                    }
                };
                let Ok((file, mut buf, res)) = out else {
                    return Poll::Ready(Some(Err(())));
                };
                self.file = Some(file);
                match res {
                    Ok(0) => {
                        tracing::warn!(target: "http", "the file shrank mid-send");
                        return Poll::Ready(Some(Err(())));
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        self.offset += n as u64;
                        self.remaining -= n as u64;
                        return Poll::Ready(Some(Ok(Bytes::from_owner(PooledBuf { buf }))));
                    }
                    Err(e) => {
                        tracing::warn!(target: "http", "sendFile read failed: {e}");
                        return Poll::Ready(Some(Err(())));
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
            self.in_flight = Some(tokio::task::spawn_blocking(move || {
                let res = file.read_at(&mut buf, offset);
                (file, buf, res)
            }));
        }
    }
}

impl Drop for ActiveFile {
    fn drop(&mut self) {
        if let Some(handle) = self.in_flight.take() {
            handle.abort();
        }
    }
}

enum StreamEvent {
    Chunk(Bytes),
    End { truncated: bool },
    Abort,
}

enum StreamSource {
    Php {
        rx: mpsc::Receiver<Chunk>,
        active_file: Option<ActiveFile>,
    },
    Middleware {
        first: Option<Bytes>,
        rest: Option<Box<dyn BodyStream>>,
    },
    Truncated {
        data: Option<Bytes>,
    },
}

impl StreamSource {
    fn stop(&mut self) {
        match self {
            Self::Php { active_file, .. } => *active_file = None,
            Self::Middleware { first, rest } => {
                *first = None;
                *rest = None;
            }
            Self::Truncated { data } => *data = None,
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<StreamEvent> {
        match self {
            Self::Php { rx, active_file } => loop {
                if let Some(af) = active_file {
                    match af.poll_next(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(chunk))) => {
                            return Poll::Ready(StreamEvent::Chunk(chunk));
                        }
                        Poll::Ready(Some(Err(()))) => {
                            *active_file = None;
                            return Poll::Ready(StreamEvent::Abort);
                        }
                        Poll::Ready(None) => {
                            *active_file = None;
                            continue;
                        }
                    }
                }
                match rx.poll_recv(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(None) => return Poll::Ready(StreamEvent::Abort),
                    Poll::Ready(Some(Chunk::Data(chunk))) if chunk.is_empty() => continue,
                    Poll::Ready(Some(Chunk::Data(chunk))) => {
                        return Poll::Ready(StreamEvent::Chunk(chunk));
                    }
                    Poll::Ready(Some(Chunk::File { file, offset, len })) => {
                        if len > 0 {
                            *active_file = Some(ActiveFile::new(file, offset, len));
                        }
                    }
                    Poll::Ready(Some(Chunk::End { truncated })) => {
                        return Poll::Ready(StreamEvent::End { truncated });
                    }
                }
            },
            Self::Middleware { first, rest } => loop {
                if let Some(chunk) = first.take() {
                    if chunk.is_empty() {
                        continue;
                    }
                    return Poll::Ready(StreamEvent::Chunk(chunk));
                }
                let Some(stream) = rest.as_mut() else {
                    return Poll::Ready(StreamEvent::End { truncated: false });
                };
                match Pin::new(stream.as_mut()).poll_next_chunk(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(None) => {
                        *rest = None;
                        return Poll::Ready(StreamEvent::End { truncated: false });
                    }
                    Poll::Ready(Some(Ok(chunk))) if chunk.is_empty() => continue,
                    Poll::Ready(Some(Ok(chunk))) => {
                        return Poll::Ready(StreamEvent::Chunk(chunk));
                    }
                    Poll::Ready(Some(Err(e))) => {
                        tracing::error!(target: "http", "middleware stream read failed: {e}");
                        *rest = None;
                        return Poll::Ready(StreamEvent::Abort);
                    }
                }
            },
            Self::Truncated { data } => {
                if let Some(chunk) = data.take()
                    && !chunk.is_empty()
                {
                    return Poll::Ready(StreamEvent::Chunk(chunk));
                }
                Poll::Ready(StreamEvent::Abort)
            }
        }
    }
}

/// Unified streaming body handling PHP chunks/files, middleware streams, and truncated responses.
pub(crate) struct StreamBody {
    source: StreamSource,
    encoders: Vec<Box<dyn BodyEncoder>>,
    declared_len: Option<u64>,
    sent_raw: u64,
    pending_error: bool,
    finished: bool,
}

impl StreamBody {
    pub(super) fn php(
        rx: mpsc::Receiver<Chunk>,
        encoders: Vec<Box<dyn BodyEncoder>>,
        declared_len: Option<u64>,
    ) -> Self {
        Self::new(
            StreamSource::Php {
                rx,
                active_file: None,
            },
            encoders,
            declared_len,
        )
    }

    pub(super) fn middleware(
        first: Bytes,
        rest: Box<dyn BodyStream>,
        encoders: Vec<Box<dyn BodyEncoder>>,
        declared_len: Option<u64>,
    ) -> Self {
        Self::new(
            StreamSource::Middleware {
                first: Some(first),
                rest: Some(rest),
            },
            encoders,
            declared_len,
        )
    }

    pub(super) fn truncated(data: Option<Bytes>, declared_len: Option<u64>) -> Self {
        Self::new(StreamSource::Truncated { data }, Vec::new(), declared_len)
    }

    fn new(
        source: StreamSource,
        encoders: Vec<Box<dyn BodyEncoder>>,
        declared_len: Option<u64>,
    ) -> Self {
        Self {
            source,
            encoders,
            declared_len,
            sent_raw: 0,
            pending_error: false,
            finished: false,
        }
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
        if this.pending_error {
            this.finished = true;
            return Poll::Ready(Some(Err(StreamAbort)));
        }

        loop {
            match this.source.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(StreamEvent::Chunk(chunk)) => {
                    this.sent_raw += chunk.len() as u64;
                    let reached = this.declared_len == Some(this.sent_raw);
                    if this.encoders.is_empty() {
                        if reached {
                            this.source.stop();
                            this.finished = true;
                        }
                        return Poll::Ready(Some(Ok(Frame::data(chunk))));
                    }
                    if reached {
                        let encoded = match run_encoders(&mut this.encoders, &chunk, true) {
                            Ok(b) => b,
                            Err(_) => return this.trigger_abort(cx),
                        };
                        this.source.stop();
                        this.finished = true;
                        return Poll::Ready(
                            (!encoded.is_empty()).then(|| Ok(Frame::data(encoded))),
                        );
                    }
                    let encoded = match run_encoders(&mut this.encoders, &chunk, false) {
                        Ok(b) => b,
                        Err(_) => return this.trigger_abort(cx),
                    };
                    if !encoded.is_empty() {
                        return Poll::Ready(Some(Ok(Frame::data(encoded))));
                    }
                }
                Poll::Ready(StreamEvent::End { truncated }) => {
                    if truncated || this.declared_len.is_some_and(|cl| this.sent_raw < cl) {
                        return this.trigger_abort(cx);
                    }
                    let tail = if this.encoders.is_empty() {
                        Bytes::new()
                    } else {
                        match run_encoders(&mut this.encoders, &[], true) {
                            Ok(t) => t,
                            Err(_) => return this.trigger_abort(cx),
                        }
                    };
                    this.finished = true;
                    return Poll::Ready((!tail.is_empty()).then(|| Ok(Frame::data(tail))));
                }
                Poll::Ready(StreamEvent::Abort) => return this.trigger_abort(cx),
            }
        }
    }
}

/// Concrete unboxed `http_body::Body` returned to `hyper`.
pub(crate) enum ResponseBody {
    Empty,
    Once(Option<Bytes>),
    Stream(Box<StreamBody>),
}

impl ResponseBody {
    /// True when the entire body is already materialized in memory and cannot abort mid-stream.
    #[inline]
    pub(crate) fn is_complete(&self) -> bool {
        !matches!(self, Self::Stream(_))
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = StreamAbort;

    #[inline]
    fn is_end_stream(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Once(data) => data.is_none(),
            Self::Stream(s) => s.is_end_stream(),
        }
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Empty => SizeHint::with_exact(0),
            Self::Once(data) => SizeHint::with_exact(data.as_ref().map_or(0, |b| b.len() as u64)),
            Self::Stream(s) => s.size_hint(),
        }
    }

    #[inline]
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            Self::Empty => Poll::Ready(None),
            Self::Once(data) => match data.take() {
                Some(b) if !b.is_empty() => Poll::Ready(Some(Ok(Frame::data(b)))),
                _ => Poll::Ready(None),
            },
            Self::Stream(s) => Pin::new(s.as_mut()).poll_frame(cx),
        }
    }
}
