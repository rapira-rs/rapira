//! The unary call that the plugin hands to PHP and its reply.

use connectrpc::http::HeaderMap;
use rapira_sapi::types::Addr;

/// One unary RPC. `message` is the binary protobuf encoding of the method's input message.
pub(crate) struct UnaryCall {
    /// `package.Service/Method`, without a leading slash.
    pub method: String,
    pub protocol: connectrpc::Protocol,
    /// The request headers as received. `getContext()` drops the transport names and decodes `-bin` values when it builds `Context::$metadata`.
    pub metadata: HeaderMap,
    /// Unix seconds.
    pub deadline: Option<f64>,
    pub remote: Addr,
    pub message: bytes::Bytes,
}

/// The outcome of a unary RPC. The metadata is in wire form: `-bin` values are unpadded base64.
pub(crate) struct UnaryReply {
    pub headers: HeaderMap,
    pub trailers: HeaderMap,
    /// The output message, or the status the call failed with.
    pub outcome: std::result::Result<bytes::Bytes, connectrpc::ConnectError>,
}
