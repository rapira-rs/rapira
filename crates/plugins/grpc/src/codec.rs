use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Buf, BufMut, Bytes};
use extension_api::{Body, BoxError};
use http::{HeaderMap, HeaderValue};
use http_body::{Body as HttpBody, Frame};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

pub(crate) struct RawCodec;
pub(crate) struct RawEncoder;
pub(crate) struct RawDecoder;

impl Codec for RawCodec {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;

    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }
    fn decoder(&mut self) -> RawDecoder {
        RawDecoder
    }
}

impl Encoder for RawEncoder {
    type Item = Bytes;
    type Error = Status;

    fn encode(&mut self, item: Bytes, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawDecoder {
    type Item = Bytes;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Bytes>, Status> {
        Ok(Some(src.copy_to_bytes(src.remaining())))
    }
}

pub(crate) struct IdentityBody {
    prefix: Option<[u8; 5]>,
    message: Option<Bytes>,
    ended: bool,
}

impl IdentityBody {
    pub fn new(message: Bytes) -> Result<Self, Status> {
        let length = u32::try_from(message.len())
            .map_err(|_| Status::out_of_range("Response message exceeds gRPC envelope limit"))?;
        let mut prefix = [0; 5];
        prefix[1..].copy_from_slice(&length.to_be_bytes());
        Ok(Self {
            prefix: Some(prefix),
            message: Some(message),
            ended: false,
        })
    }
}

impl HttpBody for IdentityBody {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        let frame = if let Some(prefix) = self.prefix.take() {
            Frame::data(Bytes::copy_from_slice(&prefix))
        } else if let Some(message) = self.message.take().filter(|message| !message.is_empty()) {
            Frame::data(message)
        } else if !self.ended {
            self.ended = true;
            let mut trailers = HeaderMap::new();
            trailers.insert("grpc-status", HeaderValue::from_static("0"));
            Frame::trailers(trailers)
        } else {
            return Poll::Ready(None);
        };
        Poll::Ready(Some(Ok(frame)))
    }

    fn is_end_stream(&self) -> bool {
        self.ended
    }
}

pub(crate) struct EnvelopeBody {
    inner: Body,
    prefix: [u8; 5],
    prefix_len: usize,
    remaining: usize,
    unary: bool,
    seen_message: bool,
    ended: bool,
}

impl EnvelopeBody {
    pub fn unary(inner: Body) -> Self {
        Self {
            unary: true,
            ..Self::streaming(inner)
        }
    }

    pub fn streaming(inner: Body) -> Self {
        Self {
            inner,
            prefix: [0; 5],
            prefix_len: 0,
            remaining: 0,
            unary: false,
            seen_message: false,
            ended: false,
        }
    }

    fn consume(&mut self, mut data: &[u8]) -> Result<(), Status> {
        while !data.is_empty() {
            if self.remaining != 0 {
                let count = self.remaining.min(data.len());
                self.remaining -= count;
                data = &data[count..];
            } else {
                if self.unary && self.seen_message {
                    return Err(Status::internal("Expected one request message"));
                }
                let start = self.prefix_len;
                let count = (5 - start).min(data.len());
                self.prefix[start..start + count].copy_from_slice(&data[..count]);
                self.prefix_len += count;
                data = &data[count..];
                if self.prefix_len == 5 {
                    self.remaining =
                        u32::from_be_bytes(self.prefix[1..5].try_into().unwrap()) as usize;
                    self.prefix_len = 0;
                    self.seen_message = true;
                }
            }
        }
        Ok(())
    }
}

// Check wire lengths through EOF or trailers. Tonic validates flags and decodes payloads.
// https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
impl HttpBody for EnvelopeBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        let frame = ready!(Pin::new(&mut self.inner).poll_frame(cx));
        if let Some(Ok(frame)) = &frame
            && let Some(data) = frame.data_ref()
        {
            if let Err(status) = self.consume(data) {
                self.ended = true;
                return Poll::Ready(Some(Err(status.into())));
            }
        } else {
            self.ended = true;
            if !matches!(frame, Some(Err(_)))
                && (self.prefix_len != 0
                    || self.remaining != 0
                    || (self.unary && !self.seen_message))
            {
                return Poll::Ready(Some(Err(
                    Status::internal("Incomplete request message").into()
                )));
            }
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.ended
    }
}
