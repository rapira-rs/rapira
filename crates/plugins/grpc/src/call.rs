use http::HeaderMap;
use rapira_sapi::types::Addr;
use rapira_sapi::work::{Held, Work, now_unix_f64};
use rapira_sapi::zend_object;
use tokio::sync::oneshot;

use crate::php::{GrpcState, grpc_call_from};

/// One unary RPC. `message` is the binary protobuf encoding of the method's input message.
#[derive(Debug)]
pub(crate) struct UnaryCall {
    /// `package.Service/Method`, without a leading slash.
    pub method: String,
    pub protocol: RpcProtocol,
    /// The request headers as received. `getContext()` drops the transport names and decodes `-bin` values when it builds `Context::$metadata`.
    pub metadata: http::HeaderMap,
    /// Unix seconds.
    pub deadline: Option<f64>,
    pub remote: Addr,
    pub message: bytes::Bytes,
}

/// The protocol that the client of an RPC used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcProtocol {
    Grpc,
    GrpcWeb,
    Connect,
}

/// The outcome of a unary RPC. The metadata is in wire form: `-bin` values are unpadded base64.
#[derive(Debug, PartialEq)]
pub(crate) struct UnaryReply {
    pub headers: http::HeaderMap,
    pub trailers: http::HeaderMap,
    /// The output message, or the status the call failed with.
    pub outcome: std::result::Result<bytes::Bytes, RpcStatus>,
}

/// `google.rpc.Status`. `code` is 1..=16. Each detail is a (type URL, packed message) pair. https://github.com/googleapis/googleapis/blob/master/google/rpc/status.proto
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RpcStatus {
    pub code: u32,
    pub message: String,
    pub details: Vec<(String, bytes::Bytes)>,
}

/// The gRPC status of a boot-failed worker's shed call: https://github.com/grpc/grpc/blob/master/doc/statuscodes.md
const GRPC_UNAVAILABLE: u32 = 14;

/// One unary gRPC call on the intake.
pub(crate) struct Call {
    pub(crate) call: UnaryCall,
    /// Unix timestamp of the enqueue.
    pub(crate) received_at: f64,
    pub(crate) reply: oneshot::Sender<UnaryReply>,
}

impl Call {
    /// A dropped sender means that PHP lost the call; dropping the receiver closes the call for PHP.
    pub(crate) fn new(call: UnaryCall) -> (Self, oneshot::Receiver<UnaryReply>) {
        let (reply, rx) = oneshot::channel();
        let call = Self {
            call,
            received_at: now_unix_f64(),
            reply,
        };
        (call, rx)
    }

    /// Commits the outcome, as a finalize from PHP does.
    pub(crate) fn respond(self, reply: UnaryReply) {
        let _ = self.reply.send(reply);
    }
}

impl Work for Call {
    fn cancelled(&self) -> bool {
        self.reply.is_closed()
    }

    unsafe fn attach(self: Box<Self>, obj: *mut zend_object) -> *mut dyn Held {
        let ptr = Box::into_raw(Box::new(GrpcState::new(*self)));
        // SAFETY: the caller passes a live Rapira\Internal\Grpc\UnaryCall object.
        unsafe { (*grpc_call_from(obj)).state = ptr.cast() };
        ptr
    }

    fn into_cgi(self: Box<Self>) -> Option<rapira_sapi::types::Context> {
        None
    }

    fn shed(self: Box<Self>) {
        self.respond(UnaryReply {
            headers: HeaderMap::new(),
            trailers: HeaderMap::new(),
            outcome: Err(RpcStatus {
                code: GRPC_UNAVAILABLE,
                message: "the worker failed to boot".into(),
                details: Vec::new(),
            }),
        });
    }
}
