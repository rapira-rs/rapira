//! Pure Rust gRPC call state machine, metadata codec, and `ErrorDetail` normalization.

#[cfg(test)]
mod tests;

use std::ffi::CStr;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD_NO_PAD};
use bytes::Bytes;
use connectrpc::http::header::{HeaderMap, HeaderName, HeaderValue};
use connectrpc::{ConnectError, ErrorDetail, Protocol};
use rapira_sapi::pool::{Event, record};
use rapira_sapi::types::Addr;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy)]
pub(crate) struct Deadline {
    pub at: Instant,
    pub unix: f64,
}

pub(crate) fn expired(until: Option<Instant>) -> bool {
    until.is_some_and(|at| Instant::now() >= at)
}

/// One call with one request message. `message` is the binary protobuf encoding of the method's input message.
pub(crate) struct Call {
    /// `package.Service/Method`, without a leading slash.
    pub method: Arc<str>,
    pub protocol: Protocol,
    /// The request headers as received. `context_metadata()` drops the transport names and decodes `-bin` values.
    pub metadata: HeaderMap,
    pub deadline: Option<Deadline>,
    pub remote: Addr,
    pub message: Bytes,
    pub received_at: f64,
}

impl Call {
    pub(crate) fn until(&self) -> Option<Instant> {
        self.deadline.map(|d| d.at)
    }
}

/// The outcome of a unary call. The metadata is in wire form, where `-bin` values are unpadded base64.
pub(crate) struct UnaryReply {
    pub headers: HeaderMap,
    pub trailers: HeaderMap,
    /// The output message, or the status the call failed with.
    pub outcome: Result<Bytes, ConnectError>,
}

/// The start of a streaming response, in wire form. `trailers` are the trailers of a stream that ends with success.
#[derive(Debug)]
pub(crate) struct Head {
    pub headers: HeaderMap,
    pub trailers: HeaderMap,
}

/// One step of a streaming response after its head. A sender that drops with no `End` and no `Fail` means that PHP lost the call.
#[derive(Debug)]
pub(crate) enum Part {
    /// The binary protobuf encoding of one output message.
    Message(Bytes),
    End,
    /// The status that the call failed with. It carries the trailers.
    Fail(ConnectError),
}

/// One metadata name with values in arrival or add order. `-bin` values are raw bytes.
#[derive(Debug, PartialEq)]
pub struct MetadataField {
    pub name: HeaderName,
    pub values: Vec<Vec<u8>>,
}

/// Request `-bin` values use the standard alphabet with or without padding. Implementations "MUST accept padded and un-padded values". See https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
const BIN_DECODE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Names that the transport owns: gRPC and Connect control fields, content framing, Connect trailers in the head, and the HTTP connection-specific fields. `name` is lower-case.
fn reserved(name: &str) -> bool {
    const PREFIXES: [&str; 4] = ["grpc-", "connect-", "content-", "trailer-"];
    const NAMES: [&str; 9] = [
        "te",
        "trailer",
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
        "host",
        "accept-encoding",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p)) || NAMES.contains(&name)
}

/// The Metadata rule for a text value: printable ASCII, 0x20 to 0x7E.
pub(crate) fn printable(value: &[u8]) -> bool {
    value.iter().all(|b| (0x20..=0x7e).contains(b))
}

/// A metadata name that carries binary values.
pub(crate) fn is_binary(name: &[u8]) -> bool {
    name.ends_with(b"-bin")
}

/// Builds `Context::$metadata` from the keys that the transport does not own, with `-bin` values decoded. It drops a text value that is not printable and an undecodable `-bin` value. A key with no values left is absent.
/// A `-bin` value is split on "," first: "Implementations must split Binary-Headers on "," before decoding the Base64-encoded values." https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
/// The optional whitespace around "," follows the HTTP list rule. https://www.rfc-editor.org/rfc/rfc9110#section-5.6.1
pub fn context_metadata(headers: &HeaderMap) -> Vec<MetadataField> {
    let mut out = Vec::new();
    for name in headers.keys() {
        if reserved(name.as_str()) {
            continue;
        }
        let binary = is_binary(name.as_str().as_bytes());
        let mut values: Vec<Vec<u8>> = Vec::new();
        for v in headers.get_all(name) {
            let v = v.as_bytes();
            if !binary {
                if printable(v) {
                    values.push(v.to_vec());
                } else {
                    tracing::debug!(target: "grpc", "dropped a non-printable {name} value");
                }
                continue;
            }
            for piece in v.split(|&b| b == b',').map(<[u8]>::trim_ascii) {
                match BIN_DECODE.decode(piece) {
                    Ok(decoded) => values.push(decoded),
                    Err(e) => {
                        tracing::debug!(target: "grpc", "dropped an undecodable {name} value: {e}");
                    }
                }
            }
        }
        if !values.is_empty() {
            out.push(MetadataField {
                name: name.clone(),
                values,
            });
        }
    }
    out
}

/// Response metadata in wire form. `-bin` values are base64 without padding, because implementations "should emit un-padded values" (PROTOCOL-HTTP2 Requests).
/// A text value loses its edge spaces. An HTTP/2 field value "MUST NOT start or end with an ASCII whitespace character" (https://www.rfc-editor.org/rfc/rfc9113#section-8.2.1), and for gRPC "it may be stripped" (PROTOCOL-HTTP2 Requests).
pub(crate) fn wire(fields: &[MetadataField]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for MetadataField { name, values } in fields {
        let binary = is_binary(name.as_str().as_bytes());
        for v in values {
            let value = if binary {
                HeaderValue::try_from(STANDARD_NO_PAD.encode(v))
            } else {
                HeaderValue::from_bytes(v.trim_ascii())
            };
            map.append(
                name.clone(),
                value.expect("add accepts only printable ASCII text, and base64 is ASCII"),
            );
        }
    }
    map
}

/// Normalizes a PHP `ErrorDetail` into a `connectrpc::ErrorDetail` with unpadded base64 value bytes.
/// A Connect detail carries the bare type name; gRPC and gRPC-Web keep the `google.protobuf.Any` type URL whole, because connectrpc adds `type.googleapis.com/` only to a name without `/`.
pub(crate) fn normalize_error_detail(
    protocol: Protocol,
    type_url: &[u8],
    value: &[u8],
) -> ErrorDetail {
    let whole_url = matches!(protocol, Protocol::Grpc | Protocol::GrpcWeb);
    let url = String::from_utf8_lossy(type_url);
    ErrorDetail {
        type_url: match url.rsplit_once('/') {
            Some((_, name)) if !whole_url => name.to_owned(),
            _ => url.into_owned(),
        },
        value: Some(STANDARD_NO_PAD.encode(value)),
        debug: None,
    }
}

/// Where the outcome of a call goes.
pub(crate) enum Reply {
    Unary(oneshot::Sender<UnaryReply>),
    Stream(StreamReply),
}

/// The two senders of a streaming response. The handler holds both receivers until the head arrives, then the response stream holds the receiver of `parts`. So a closed `parts` means in each phase that the plugin closed the call.
pub(crate) struct StreamReply {
    /// None after the head went out.
    head: Option<oneshot::Sender<Result<Head, ConnectError>>>,
    parts: mpsc::Sender<Part>,
    /// A trailer was added after the head went out. A stream that ends with success does not send it.
    late_trailer: bool,
}

impl StreamReply {
    pub(crate) fn new(
        head: oneshot::Sender<Result<Head, ConnectError>>,
        parts: mpsc::Sender<Part>,
    ) -> Self {
        Self {
            head: Some(head),
            parts,
            late_trailer: false,
        }
    }
}

/// Outcome returned by pure `CallState` operations. The PHP FFI adapter throws the corresponding exception after dropping owned state.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    Ok,
    Finalized,
    Discarded,
    HeadersCommitted,
    BadField(&'static CStr),
}

/// Sends the head when it is not out yet, with both metadata halves as they are now. Returns false when the plugin closed the call.
fn commit(out: &mut StreamReply, headers: &[MetadataField], trailers: &[MetadataField]) -> bool {
    let Some(head) = out.head.take() else {
        return true;
    };
    head.send(Ok(Head {
        headers: wire(headers),
        trailers: wire(trailers),
    }))
    .is_ok()
}

/// Pure Rust state machine for one unary or server-streaming gRPC call.
pub(crate) struct CallState {
    req: Call,
    /// None after the outcome went out or the call was discarded. The call is then finalized.
    reply: Option<Reply>,
    discarded: bool,
    headers: Vec<MetadataField>,
    trailers: Vec<MetadataField>,
    /// Cached decoded metadata for `Context::$metadata` so Zend OOM bailout frames hold no `Drop` values.
    context_metadata: Option<Vec<MetadataField>>,
}

impl CallState {
    /// Creates one open call state. A dropped `reply` reports a lost call as `INTERNAL` to the client.
    pub(crate) fn new(req: Call, reply: Reply) -> Self {
        Self {
            req,
            reply: Some(reply),
            discarded: false,
            headers: Vec::new(),
            trailers: Vec::new(),
            context_metadata: None,
        }
    }

    pub(crate) fn request(&self) -> &Call {
        &self.req
    }

    pub(crate) fn is_stream(&self) -> bool {
        matches!(self.reply, Some(Reply::Stream(_)))
    }

    pub(crate) fn settled(&self) -> bool {
        self.reply.is_none()
    }

    /// A pending reply needs both an open receiver and a live call deadline.
    pub(crate) fn cancelled(&self) -> bool {
        self.discarded
            || self.reply.as_ref().is_some_and(|reply| {
                let closed = match reply {
                    Reply::Unary(tx) => tx.is_closed(),
                    Reply::Stream(out) => out.parts.is_closed(),
                };
                closed || expired(self.req.until())
            })
    }

    pub(crate) fn headers(&self) -> &[MetadataField] {
        &self.headers
    }

    pub(crate) fn trailers(&self) -> &[MetadataField] {
        &self.trailers
    }

    pub(crate) fn context_metadata(&mut self) -> &[MetadataField] {
        self.context_metadata
            .get_or_insert_with(|| context_metadata(&self.req.metadata))
    }

    pub(crate) fn release_context(&mut self) {
        self.context_metadata = None;
        self.req.metadata.clear();
    }

    /// Marks an open call discarded, drops its reply channel, and records an errored pool outcome once.
    pub(crate) fn discard(&mut self) {
        if self.settled() {
            return;
        }
        self.discarded = true;
        self.reply = None;
        record(Event::Handled { errored: true });
    }

    /// Rejects a queued call whose worker failed to boot with `UNAVAILABLE`.
    pub(crate) fn shed(self) {
        let err = ConnectError::unavailable("the worker failed to boot");
        match self.reply {
            Some(Reply::Unary(reply)) => {
                let _ = reply.send(UnaryReply {
                    headers: HeaderMap::new(),
                    trailers: HeaderMap::new(),
                    outcome: Err(err),
                });
            }
            Some(Reply::Stream(StreamReply {
                head: Some(head), ..
            })) => {
                let _ = head.send(Err(err));
            }
            _ => {}
        }
    }

    /// Validates one response metadata entry, then adds it to its half. Checks the entry before call state so a bad entry gives `ValueError` on a finalized call too.
    pub(crate) fn add(&mut self, trailer: bool, binary: bool, name: &[u8], value: &[u8]) -> Verb {
        let Ok(name) = HeaderName::from_bytes(name) else {
            return Verb::BadField(c"the metadata name is not a valid header name");
        };
        // The Header-Name rule. See https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
        if !name
            .as_str()
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'_' | b'-' | b'.'))
        {
            return Verb::BadField(c"a metadata name must use only 0-9 a-z _ - .");
        }
        if reserved(name.as_str()) {
            return Verb::BadField(c"the metadata name is reserved for the transport");
        }
        match (binary, is_binary(name.as_str().as_bytes())) {
            (false, true) => {
                return Verb::BadField(
                    c"a metadata name with the -bin suffix takes binary values: use addBinaryHeader() or addBinaryTrailer()",
                );
            }
            (true, false) => {
                return Verb::BadField(c"a binary metadata name must have the -bin suffix");
            }
            _ => {}
        }
        if !binary && !printable(value) {
            return Verb::BadField(c"a text metadata value must be printable ASCII");
        }
        // A call that the plugin closed keeps the entry, and finish() gives WorkDiscardedException.
        if self.settled() {
            return Verb::Finalized;
        }
        // The first yield of a streaming response sent the headers. It also fixed the trailers of a stream that ends with success; fail() sends a later trailer.
        if let Some(Reply::Stream(out)) = self.reply.as_mut()
            && out.head.is_none()
        {
            if !trailer {
                return Verb::HeadersCommitted;
            }
            out.late_trailer = true;
        }
        let half = if trailer {
            &mut self.trailers
        } else {
            &mut self.headers
        };
        match half.iter_mut().find(|field| field.name == name) {
            Some(field) => field.values.push(value.to_vec()),
            None => half.push(MetadataField {
                name,
                values: vec![value.to_vec()],
            }),
        }
        Verb::Ok
    }

    fn settle(&mut self, sent: bool) -> Verb {
        if sent {
            record(Event::Handled { errored: false });
            Verb::Ok
        } else {
            self.discarded = true;
            record(Event::Handled { errored: true });
            Verb::Discarded
        }
    }

    /// Sends the one outcome of the call, with both metadata halves in wire form. On a streaming response the outcome is a status: before the head it goes out with both halves; after the head it ends the stream with every trailer.
    pub(crate) fn finish(
        &mut self,
        result: Result<&[u8], ConnectError>,
        send_part: impl FnOnce(&mpsc::Sender<Part>, Part, Option<Instant>) -> bool,
    ) -> Verb {
        if self.cancelled() {
            self.discard();
            return Verb::Discarded;
        }
        let Some(reply) = self.reply.take() else {
            return Verb::Finalized;
        };
        let trailers = wire(&self.trailers);
        // The receiver can drop after the cancelled() check.
        let sent = match (reply, result) {
            (Reply::Unary(reply), outcome) => reply
                .send(UnaryReply {
                    headers: wire(&self.headers),
                    trailers,
                    outcome: outcome.map(Bytes::copy_from_slice),
                })
                .is_ok(),
            (Reply::Stream(mut out), Err(err)) => match out.head.take() {
                Some(head) => head
                    .send(Err(err
                        .with_headers(wire(&self.headers))
                        .with_trailers(trailers)))
                    .is_ok(),
                None => send_part(
                    &out.parts,
                    Part::Fail(err.with_trailers(trailers)),
                    self.req.until(),
                ),
            },
            (Reply::Stream(_), Ok(_)) => unreachable!("only UnaryCall has respond(string)"),
        };
        self.settle(sent)
    }

    /// Pre-generator check for `ServerStreamingCall::respond(\Generator)`.
    pub(crate) fn begin(&mut self) -> Verb {
        if self.cancelled() {
            self.discard();
            return Verb::Discarded;
        }
        if self.settled() {
            return Verb::Finalized;
        }
        Verb::Ok
    }

    /// Sends one message of a streaming response. The first message commits the head before it. Returns false when the message has no destination and discards the call.
    pub(crate) fn send(
        &mut self,
        message: &[u8],
        send_part: impl FnOnce(&mpsc::Sender<Part>, Part, Option<Instant>) -> bool,
    ) -> bool {
        if self.cancelled() {
            self.discard();
            return false;
        }
        // respond() or fail() inside the running generator finalized the call, so the generator gets the discard exception at its yield.
        let Some(Reply::Stream(out)) = self.reply.as_mut() else {
            return false;
        };
        let sent = commit(out, &self.headers, &self.trailers)
            && send_part(
                &out.parts,
                Part::Message(Bytes::copy_from_slice(message)),
                self.req.until(),
            );
        if !sent {
            self.discard();
            return false;
        }
        true
    }

    /// After the generator ends, sends the head of a stream with no message, then the end of the stream.
    pub(crate) fn end(
        &mut self,
        send_part: impl FnOnce(&mpsc::Sender<Part>, Part, Option<Instant>) -> bool,
    ) {
        if self.cancelled() {
            self.discard();
            return;
        }
        // fail() inside the running generator finalized the call.
        let Some(Reply::Stream(mut out)) = self.reply.take() else {
            return;
        };
        let sent = commit(&mut out, &self.headers, &self.trailers)
            && send_part(&out.parts, Part::End, self.req.until());
        if out.late_trailer {
            tracing::debug!(
                target: "grpc",
                "{}: a trailer added after the first yield is not sent",
                self.req.method
            );
        }
        let _ = self.settle(sent);
    }
}
