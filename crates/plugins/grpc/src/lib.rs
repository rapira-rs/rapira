//! The grpc plugin. It serves unary and server-streaming calls of the configured services over gRPC, gRPC-Web and Connect on one listener. PHP answers them in dispatcher mode. The plugin answers health and reflection without PHP.

use std::sync::Arc;
use std::time::Duration;

use connectrpc::{ConnectionConfig, DeadlinePolicy, Router};
use connectrpc_health::Status;
use rapira_net::PreparedListener;
use rapira_sapi::plugin::{Plugin, ServeFuture};
use rapira_sapi::rapira_plugin;
use rapira_sapi::work::Intake;
use tokio::sync::watch;

mod call;
pub mod config;
mod dispatch;
mod health;
pub mod php;
pub mod schema;
mod serve;

use call::{Call, Head, Part, UnaryReply};
pub use php::rapira_grpc_register_classes;
use schema::{MethodInfo, Schema};

/// The listener and the values that `from_settings` loads before PHP starts.
pub struct Server {
    listener: PreparedListener,
    schema: Arc<Schema>,
    /// The routes that the plugin answers without PHP: health and, when configured, reflection.
    router: Router,
    health: watch::Sender<Status>,
    /// The timeout of a call whose client sets no deadline, and the longest timeout that a client can set.
    deadlines: DeadlinePolicy,
    /// The HTTP/2 PING interval and the wait for the PING ACK. A peer that is gone without a FIN sends no ACK. So its connection closes within the sum of both values and does not hold a later drain.
    connection: ConnectionConfig,
    /// The configured order. The first interceptor runs first.
    interceptors: Vec<Arc<dyn connectrpc::Interceptor>>,
}

impl Plugin for Server {
    fn name(&self) -> &'static str {
        "grpc"
    }

    fn dispatcher(&self) -> *const rapira_plugin {
        &raw const php::rapira_grpc_plugin
    }

    fn worker_init(&self) -> Arc<dyn Fn() + Send + Sync> {
        let schema = Arc::clone(&self.schema);
        Arc::new(move || schema::enter(Arc::clone(&schema)))
    }

    fn serve(
        self: Box<Self>,
        intake: Intake,
        stop: watch::Receiver<bool>,
        drain_grace: Duration,
    ) -> ServeFuture {
        Box::pin(serve::serve(*self, intake, stop, drain_grace))
    }
}
