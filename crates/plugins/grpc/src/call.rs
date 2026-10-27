use connectrpc::http::HeaderMap;
use rapira_sapi::types::Addr;

/// One unary RPC. `message` is the binary protobuf encoding of the method's input message.
#[derive(Debug)]
pub(crate) struct UnaryCall {
    /// `package.Service/Method`, without a leading slash.
    pub method: String,
    pub protocol: Option<connectrpc::Protocol>,
    /// The request headers as received. `getContext()` drops the transport names and decodes `-bin` values when it builds `Context::$metadata`.
    pub metadata: HeaderMap,
    /// Unix seconds.
    pub deadline: Option<f64>,
    pub remote: Addr,
    pub message: bytes::Bytes,
}

/// The outcome of a unary RPC. The metadata is in wire form: `-bin` values are unpadded base64.
#[derive(Debug, PartialEq)]
pub(crate) struct UnaryReply {
    pub headers: HeaderMap,
    pub trailers: HeaderMap,
    /// The output message, or the status the call failed with.
    pub outcome: std::result::Result<bytes::Bytes, RpcStatus>,
}

/// `google.rpc.Status`. `code` is 1..=16. https://github.com/googleapis/googleapis/blob/master/google/rpc/status.proto
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RpcStatus {
    pub code: u32,
    pub message: String,
    pub details: Vec<StatusDetail>,
}

/// One `google.protobuf.Any` detail of a status. `value` is the packed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusDetail {
    pub type_url: String,
    pub value: bytes::Bytes,
}
