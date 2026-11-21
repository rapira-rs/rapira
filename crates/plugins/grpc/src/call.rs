//! The call that the plugin hands to PHP, and what PHP sends back.

use bytes::Bytes;
use connectrpc::http::HeaderMap;
use connectrpc::{ConnectError, MethodKind};
use rapira_sapi::types::Addr;

/// One call with one request message. `message` is the binary protobuf encoding of the method's input message.
pub(crate) struct Call {
    /// `package.Service/Method`, without a leading slash.
    pub method: String,
    pub kind: MethodKind,
    pub protocol: connectrpc::Protocol,
    /// The request headers as received. `getContext()` drops the transport names and decodes `-bin` values when it builds `Context::$metadata`.
    pub metadata: HeaderMap,
    /// Unix seconds.
    pub deadline: Option<f64>,
    pub remote: Addr,
    pub message: Bytes,
}

/// The outcome of a unary call. The metadata is in wire form, where `-bin` values are unpadded base64.
pub(crate) struct UnaryReply {
    pub headers: HeaderMap,
    pub trailers: HeaderMap,
    /// The output message, or the status the call failed with.
    pub outcome: Result<Bytes, ConnectError>,
}

/// The start of a streaming response, in wire form. `trailers` are the trailers of a stream that ends with success.
pub(crate) struct Head {
    pub headers: HeaderMap,
    pub trailers: HeaderMap,
}

/// One step of a streaming response after its head. A sender that drops with no `End` and no `Fail` means that PHP lost the call.
pub(crate) enum Part {
    /// The binary protobuf encoding of one output message.
    Message(Bytes),
    End,
    /// The status that the call failed with. It carries the trailers.
    Fail(ConnectError),
}
