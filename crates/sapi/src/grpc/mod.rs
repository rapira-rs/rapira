mod call;

pub use call::Call;

use std::cell::RefCell;

use crate::plugin::PhpPart;
use crate::types::{Addr, GrpcService};
use crate::work::DispatcherClasses;

unsafe extern "C" {
    fn rapira_grpc_register_classes();
}

/// One unary RPC. `message` is the binary protobuf encoding of the method's input message.
#[derive(Debug)]
pub struct UnaryCall {
    /// `package.Service/Method`, without a leading slash.
    pub method: String,
    pub protocol: RpcProtocol,
    /// The request headers as received. rapira_sapi drops the transport names and decodes `-bin` values when it builds `Context::$metadata`.
    pub metadata: http::HeaderMap,
    /// Unix seconds.
    pub deadline: Option<f64>,
    pub remote: Addr,
    pub message: bytes::Bytes,
}

/// The protocol that the client of an RPC used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcProtocol {
    Grpc,
    GrpcWeb,
    Connect,
}

/// The outcome of a unary RPC. The metadata is in wire form: `-bin` values are unpadded base64.
#[derive(Debug, PartialEq)]
pub struct UnaryReply {
    pub headers: http::HeaderMap,
    pub trailers: http::HeaderMap,
    /// The output message, or the status the call failed with.
    pub outcome: std::result::Result<bytes::Bytes, RpcStatus>,
}

/// `google.rpc.Status`. `code` is 1..=16. Each detail is a (type URL, packed message) pair. https://github.com/googleapis/googleapis/blob/master/google/rpc/status.proto
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcStatus {
    pub code: u32,
    pub message: String,
    pub details: Vec<(String, bytes::Bytes)>,
}

pub static DISPATCHER_CLASSES: DispatcherClasses = DispatcherClasses {
    dispatcher: || unsafe { crate::rapira_ce_internal_grpc_dispatcher },
    info: || unsafe { crate::rapira_ce_internal_grpc_dispatcher_info },
    unit: || unsafe { crate::rapira_ce_internal_grpc_unary_call },
    busy: c"receive() while a Rapira\\Grpc\\UnaryCall is unfinalized; finalize it first",
};

pub static PHP_PART: PhpPart = PhpPart {
    register: rapira_grpc_register_classes,
    dispatcher: Some(DISPATCHER_CLASSES),
};

thread_local! {
    /// The services of the gRPC pool this PHP thread serves.
    pub(crate) static SERVICES: RefCell<Option<Vec<GrpcService>>> = const { RefCell::new(None) };
}

/// Sets the services `getServices()` reports on this PHP thread.
pub fn set_services(services: Vec<GrpcService>) {
    SERVICES.set(Some(services));
}
