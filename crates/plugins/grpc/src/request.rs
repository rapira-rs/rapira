use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Bytes, BytesMut};
use connectrpc::ConnectError;
use extension_api::{Body, BoxError};
use http_body::{Body as HttpBody, Frame};
use tokio::sync::oneshot;

pub(crate) struct RequestBody {
    inner: Option<Body>,
    cancelled: oneshot::Receiver<()>,
    framed: bool,
    unary: bool,
    prefix: [u8; 5],
    prefix_len: usize,
    remaining: usize,
    seen: bool,
    failure: Option<ConnectError>,
    buffered: Buffered,
    pending: Option<Frame<Bytes>>,
    limit: usize,
    capacity_hint: usize,
}

impl RequestBody {
    pub fn new(
        inner: Body,
        cancelled: oneshot::Receiver<()>,
        framed: bool,
        unary: bool,
        limit: usize,
    ) -> Self {
        let capacity_hint = inner
            .size_hint()
            .upper()
            .map_or(0, |size| usize::try_from(size).unwrap_or(limit).min(limit));
        Self {
            inner: Some(inner),
            cancelled,
            framed,
            unary,
            prefix: [0; 5],
            prefix_len: 0,
            remaining: 0,
            seen: false,
            failure: None,
            buffered: Buffered::default(),
            pending: None,
            limit,
            capacity_hint,
        }
    }

    // Request frames permit the compression flag only.
    // https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
    // https://connectrpc.com/docs/protocol/#streaming-request
    fn validate(&mut self, data: &[u8]) -> Result<(), (usize, ConnectError)> {
        let mut offset = 0;
        while offset < data.len() {
            if self.remaining != 0 {
                let count = self.remaining.min(data.len() - offset);
                self.remaining -= count;
                offset += count;
            } else {
                if self.unary && self.seen {
                    return Err((
                        offset,
                        ConnectError::internal("Expected one request message"),
                    ));
                }
                if self.prefix_len == 0 && data[offset] > 1 {
                    return Err((
                        offset,
                        ConnectError::internal("Invalid request compression flag"),
                    ));
                }
                let start = self.prefix_len;
                let count = (5 - start).min(data.len() - offset);
                self.prefix[start..start + count].copy_from_slice(&data[offset..offset + count]);
                self.prefix_len += count;
                offset += count;
                if self.prefix_len == 5 {
                    self.remaining =
                        u32::from_be_bytes(self.prefix[1..5].try_into().unwrap()) as usize;
                    self.prefix_len = 0;
                    self.seen = true;
                }
            }
        }
        Ok(())
    }

    fn poll_validated(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(error) = self.failure.take() {
            self.inner = None;
            return Poll::Ready(Some(Err(error.into())));
        }
        if self.inner.is_none() {
            return Poll::Ready(None);
        }
        if Pin::new(&mut self.cancelled).poll(cx).is_ready() {
            self.inner = None;
            return Poll::Ready(None);
        }
        let frame = ready!(Pin::new(self.inner.as_mut().unwrap()).poll_frame(cx));
        if let Some(Ok(frame)) = &frame
            && let Some(data) = frame.data_ref()
        {
            if self.framed
                && let Err((offset, error)) = self.validate(data)
            {
                if offset > 0 {
                    self.failure = Some(error);
                    return Poll::Ready(Some(Ok(Frame::data(data.slice(..offset)))));
                }
                self.inner = None;
                return Poll::Ready(Some(Err(error.into())));
            }
        } else {
            self.inner = None;
            if self.framed
                && !matches!(frame, Some(Err(_)))
                && (self.prefix_len != 0 || self.remaining != 0 || (self.unary && !self.seen))
            {
                return Poll::Ready(Some(Err(ConnectError::internal(
                    "Incomplete request message",
                )
                .into())));
            }
        }
        Poll::Ready(frame)
    }
}

#[derive(Default)]
enum Buffered {
    #[default]
    Empty,
    One(Bytes),
    Joined(BytesMut),
}

impl Buffered {
    fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::One(bytes) => bytes.len(),
            Self::Joined(bytes) => bytes.len(),
        }
    }

    fn push(&mut self, data: Bytes, capacity_hint: usize) {
        if data.is_empty() {
            return;
        }
        match self {
            Self::Empty => *self = Self::One(data),
            Self::One(first) => {
                let mut buffer =
                    BytesMut::with_capacity(capacity_hint.max(first.len() + data.len()));
                buffer.extend_from_slice(first);
                buffer.extend_from_slice(&data);
                *self = Self::Joined(buffer);
            }
            Self::Joined(buffer) => {
                if capacity_hint > buffer.capacity() {
                    buffer.reserve(capacity_hint - buffer.len());
                }
                buffer.extend_from_slice(&data);
            }
        }
    }

    fn take(&mut self) -> Option<Bytes> {
        match std::mem::take(self) {
            Self::Empty => None,
            Self::One(bytes) => Some(bytes),
            Self::Joined(bytes) => Some(bytes.freeze()),
        }
    }
}

impl HttpBody for RequestBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(frame) = self.pending.take() {
            return Poll::Ready(Some(Ok(frame)));
        }
        if !self.unary {
            return self.poll_validated(cx);
        }
        loop {
            match ready!(self.poll_validated(cx)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        if data.len() > self.limit.saturating_sub(self.buffered.len()) {
                            self.inner = None;
                            // The outer wire-size limit must see all input bytes.
                            if let Some(buffered) = self.buffered.take() {
                                self.pending = Some(Frame::data(data));
                                return Poll::Ready(Some(Ok(Frame::data(buffered))));
                            }
                            return Poll::Ready(Some(Ok(Frame::data(data))));
                        }
                        let capacity = if self.framed && self.seen {
                            (u32::from_be_bytes(self.prefix[1..5].try_into().unwrap()) as usize)
                                .saturating_add(5)
                                .min(self.limit)
                        } else {
                            self.capacity_hint
                        };
                        self.buffered.push(data, capacity);
                    }
                    Err(frame) => {
                        if let Some(buffered) = self.buffered.take() {
                            self.pending = Some(frame);
                            return Poll::Ready(Some(Ok(Frame::data(buffered))));
                        }
                        return Poll::Ready(Some(Ok(frame)));
                    }
                },
                Some(Err(error)) => {
                    self.buffered = Buffered::Empty;
                    return Poll::Ready(Some(Err(error)));
                }
                None => {
                    return Poll::Ready(self.buffered.take().map(|data| Ok(Frame::data(data))));
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_none() && self.pending.is_none() && self.buffered.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;
    use http_body_util::{BodyExt, Limited, StreamBody};
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn fragmented_bodies_preserve_limits_and_terminal_frames() {
        enum Outcome {
            Message(&'static [u8]),
            Limit,
            Incomplete,
        }

        struct Case {
            name: &'static str,
            fragments: &'static [&'static [u8]],
            framed: bool,
            trailers: bool,
            stall: bool,
            outcome: Outcome,
        }
        let cases = [
            Case {
                name: "split_prefix",
                fragments: &[b"\0\0", b"\0\0\x04", b"\x0a\x02hi"],
                framed: true,
                trailers: false,
                stall: false,
                outcome: Outcome::Message(b"\0\0\0\0\x04\x0a\x02hi"),
            },
            Case {
                name: "empty_fragments",
                fragments: &[b"", b"\0\0\0\0\x04\x0a", b"", b"\x02hi", b""],
                framed: true,
                trailers: false,
                stall: false,
                outcome: Outcome::Message(b"\0\0\0\0\x04\x0a\x02hi"),
            },
            Case {
                name: "framed_exact_limit_with_trailers",
                fragments: &[b"\0\0\0\0\x08abcd", b"efgh"],
                framed: true,
                trailers: true,
                stall: false,
                outcome: Outcome::Message(b"\0\0\0\0\x08abcdefgh"),
            },
            Case {
                name: "unframed_exact_wire_limit",
                fragments: &[b"abcdef", b"ghijklm"],
                framed: false,
                trailers: false,
                stall: false,
                outcome: Outcome::Message(b"abcdefghijklm"),
            },
            Case {
                name: "unframed_trailers",
                fragments: &[b"ab", b"cd"],
                framed: false,
                trailers: true,
                stall: false,
                outcome: Outcome::Message(b"abcd"),
            },
            Case {
                name: "first_fragment_over_limit_before_eof",
                fragments: &[b"\0\0\0\0\x09abcdefghi"],
                framed: true,
                trailers: false,
                stall: true,
                outcome: Outcome::Limit,
            },
            Case {
                name: "second_fragment_over_limit_before_eof",
                fragments: &[b"\0\0\0\0\x09abc", b"defghi"],
                framed: true,
                trailers: false,
                stall: true,
                outcome: Outcome::Limit,
            },
            Case {
                name: "joined_buffer_over_limit_before_eof",
                fragments: &[b"\0\0\0\0\x09a", b"bc", b"defghi"],
                framed: true,
                trailers: false,
                stall: true,
                outcome: Outcome::Limit,
            },
            Case {
                name: "unframed_over_limit_before_eof",
                fragments: &[b"abc", b"def", b"ghijklmn"],
                framed: false,
                trailers: false,
                stall: true,
                outcome: Outcome::Limit,
            },
            Case {
                name: "incomplete_prefix",
                fragments: &[b"\0\0", b"\0"],
                framed: true,
                trailers: false,
                stall: false,
                outcome: Outcome::Incomplete,
            },
            Case {
                name: "incomplete_message_before_trailers",
                fragments: &[b"\0\0\0\0\x04", b"\x0a"],
                framed: true,
                trailers: true,
                stall: false,
                outcome: Outcome::Incomplete,
            },
            Case {
                name: "declared_length_exceeds_limit",
                fragments: &[b"\0\xff\xff\xff\xff", b"x"],
                framed: true,
                trailers: false,
                stall: false,
                outcome: Outcome::Incomplete,
            },
        ];
        for case in cases {
            let mut frames: Vec<Result<Frame<Bytes>, BoxError>> = case
                .fragments
                .iter()
                .map(|bytes| Ok(Frame::data(Bytes::from_static(bytes))))
                .collect();
            if case.trailers {
                let mut trailers = HeaderMap::new();
                trailers.insert("x-end", "done".parse().unwrap());
                frames.push(Ok(Frame::trailers(trailers)));
            }
            let inner = if case.stall {
                StreamBody::new(tokio_stream::iter(frames).chain(tokio_stream::pending()))
                    .boxed_unsync()
            } else {
                StreamBody::new(tokio_stream::iter(frames)).boxed_unsync()
            };
            let (_sender, receiver) = oneshot::channel();
            let body = RequestBody::new(inner, receiver, case.framed, true, 13);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                Limited::new(body, 13).collect(),
            )
            .await
            .unwrap_or_else(|_| panic!("{}: body read stalled", case.name));
            match case.outcome {
                Outcome::Message(expected) => {
                    let collected = result.unwrap();
                    if case.trailers {
                        assert_eq!(
                            collected.trailers().unwrap()["x-end"],
                            "done",
                            "{}",
                            case.name
                        );
                    } else {
                        assert!(collected.trailers().is_none(), "{}", case.name);
                    }
                    assert_eq!(collected.to_bytes().as_ref(), expected, "{}", case.name);
                }
                Outcome::Limit => assert!(
                    result.unwrap_err().is::<http_body_util::LengthLimitError>(),
                    "{}",
                    case.name
                ),
                Outcome::Incomplete => assert_eq!(
                    result.unwrap_err().downcast::<ConnectError>().unwrap().code,
                    connectrpc::ErrorCode::Internal,
                    "{}",
                    case.name
                ),
            }
        }
    }
}
