use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use extension_api::{Body, BoxError};
use http::HeaderMap;
use http_body::{Body as HttpBody, Frame, SizeHint};
use tonic::Status;

#[derive(Clone, Copy)]
pub(crate) struct Deadline {
    pub expires_at: Instant,
    pub unix: f64,
}

impl Deadline {
    pub fn expired(self) -> bool {
        self.expires_at <= Instant::now()
    }
}

pub(crate) fn parse(
    headers: &HeaderMap,
    received: Instant,
    unix: f64,
) -> Result<Option<Deadline>, Status> {
    let mut values = headers.get_all("grpc-timeout").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let invalid = || Status::invalid_argument("Invalid grpc-timeout");
    if values.next().is_some() {
        return Err(invalid());
    }
    let value = value.as_bytes();
    if !(2..=9).contains(&value.len()) {
        return Err(invalid());
    }
    let (unit, digits) = value.split_last().unwrap();
    if !digits.iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    let count = digits
        .iter()
        .fold(0_u64, |n, b| n * 10 + u64::from(b - b'0'));
    let duration = match unit {
        b'H' => Duration::from_secs(count * 3600),
        b'M' => Duration::from_secs(count * 60),
        b'S' => Duration::from_secs(count),
        b'm' => Duration::from_millis(count),
        b'u' => Duration::from_micros(count),
        b'n' => Duration::from_nanos(count),
        _ => return Err(invalid()),
    };
    Ok(Some(Deadline {
        expires_at: received.checked_add(duration).ok_or_else(invalid)?,
        unix: unix + duration.as_secs_f64(),
    }))
}

pub(crate) fn status() -> Status {
    Status::deadline_exceeded("Deadline exceeded")
}

pub(crate) struct DeadlineBody {
    inner: Option<Body>,
    deadline: Deadline,
    sleep: Pin<Box<tokio::time::Sleep>>,
    response: bool,
}

impl DeadlineBody {
    pub fn new(inner: Body, deadline: Deadline, response: bool) -> Self {
        Self {
            inner: Some(inner),
            deadline,
            sleep: Box::pin(tokio::time::sleep_until(deadline.expires_at.into())),
            response,
        }
    }
}

impl HttpBody for DeadlineBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.inner.is_none() {
            return Poll::Ready(None);
        }
        if self.deadline.expired() || self.sleep.as_mut().poll(cx).is_ready() {
            self.inner = None;
            return Poll::Ready(Some(if self.response {
                let mut trailers = HeaderMap::new();
                status()
                    .add_header(&mut trailers)
                    .map(|()| Frame::trailers(trailers))
                    .map_err(BoxError::from)
            } else {
                Err(status().into())
            }));
        }
        let poll = Pin::new(self.inner.as_mut().unwrap()).poll_frame(cx);
        if matches!(&poll, Poll::Ready(None | Some(Err(_))))
            || matches!(&poll, Poll::Ready(Some(Ok(frame))) if frame.is_trailers())
        {
            self.inner = None;
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.as_ref().is_none_or(HttpBody::is_end_stream)
    }
    fn size_hint(&self) -> SizeHint {
        self.inner
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), HttpBody::size_hint)
    }
}
