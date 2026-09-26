use bytes::Bytes;
use http::HeaderMap;
use tokio::sync::oneshot;

use super::{RpcProtocol, RpcStatus, UnaryCall, UnaryReply};
use crate::exchange::{GrpcState, grpc_call_from};
use crate::types::Addr;
use crate::work::{Held, Work, now_unix_f64};
use crate::zend_object;

/// The gRPC status of a boot-failed worker's shed call: https://github.com/grpc/grpc/blob/master/doc/statuscodes.md
const GRPC_UNAVAILABLE: u32 = 14;

/// One unary gRPC call on the intake.
pub struct Call {
    pub(crate) call: UnaryCall,
    /// Unix timestamp of the enqueue.
    pub(crate) received_at: f64,
    pub(crate) reply: oneshot::Sender<UnaryReply>,
}

impl Call {
    /// A dropped sender means that PHP lost the call; dropping the receiver closes the call for PHP.
    pub fn new(call: UnaryCall) -> (Self, oneshot::Receiver<UnaryReply>) {
        let (reply, rx) = oneshot::channel();
        let call = Self {
            call,
            received_at: now_unix_f64(),
            reply,
        };
        (call, rx)
    }

    pub fn method(&self) -> &str {
        &self.call.method
    }

    pub fn protocol(&self) -> RpcProtocol {
        self.call.protocol
    }

    pub fn metadata(&self) -> &HeaderMap {
        &self.call.metadata
    }

    /// Unix seconds.
    pub fn deadline(&self) -> Option<f64> {
        self.call.deadline
    }

    pub fn remote(&self) -> &Addr {
        &self.call.remote
    }

    pub fn message(&self) -> &Bytes {
        &self.call.message
    }

    /// Commits the outcome, as a finalize from PHP does.
    pub fn respond(self, reply: UnaryReply) {
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

    fn into_cgi(self: Box<Self>) -> Option<crate::types::Context> {
        None
    }

    fn shed(self: Box<Self>) {
        let _ = self.reply.send(UnaryReply {
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
