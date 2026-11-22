//! The grpc plugin. It serves unary and server-streaming calls of the configured services over gRPC, gRPC-Web and Connect on one listener. PHP answers them in dispatcher mode. The plugin answers health and reflection without PHP.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use connectrpc::Router;
use connectrpc_health::StaticChecker;
use connectrpc_reflection::Reflector;
use rapira_net::{ListenAddr, PreparedListener};
use rapira_sapi::plugin::{Mode, Plugin, ServeFuture, Worker};
use rapira_sapi::rapira_plugin;

mod call;
pub mod config;
mod dispatch;
pub mod php;
pub mod schema;
mod serve;

use call::{Call, Head, Part, UnaryReply};
pub use php::rapira_grpc_register_classes;
use schema::{MethodInfo, Schema, set_schema};

pub(crate) struct Config {
    pub listen: ListenAddr,
    pub schema: Arc<Schema>,
    /// Serve `grpc.reflection.v1` and `v1alpha` for the configured services.
    pub reflection: bool,
    /// The timeout of a call whose client sets no deadline.
    pub default_timeout: Option<Duration>,
    /// The longest timeout that a client can set.
    pub max_timeout: Option<Duration>,
    /// Idle time before the listener sends an HTTP/2 PING.
    pub keepalive_interval: Duration,
    /// The wait for the PING ACK. A peer that is gone without a FIN sends no ACK. So its connection closes within the sum of both values and does not hold a later drain.
    pub keepalive_timeout: Duration,
    /// The configured order. The first interceptor runs first.
    pub interceptors: Vec<Arc<dyn connectrpc::Interceptor>>,
}

pub struct Server {
    config: Config,
    prepared: Prepared,
}

/// What `prepare` leaves for `serve`: the listener and the routes that the plugin answers without PHP.
pub(crate) struct Prepared {
    pub(crate) listener: PreparedListener,
    pub(crate) router: Router,
    pub(crate) health: Arc<StaticChecker>,
}

/// Binds the listener and prepares the routes before PHP starts.
fn prepare(config: &Config) -> Result<Prepared> {
    let listener = rapira_net::bind(&config.listen)?;

    let names: Vec<String> = config
        .schema
        .services()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let (mut router, health) = connectrpc_health::install_static(Router::new(), names.clone());
    if config.reflection {
        let reflector = Reflector::from_descriptor_pool(Arc::clone(config.schema.pool()))
            .map_err(|e| anyhow!("building grpc reflection: {e}"))?
            .with_services(names);
        router = connectrpc_reflection::install(router, reflector);
    }
    set_schema(Arc::clone(&config.schema));
    tracing::info!(target: "grpc", "listening on {}", listener.addr());
    Ok(Prepared {
        listener,
        router,
        health,
    })
}

impl Plugin for Server {
    fn name(&self) -> &'static str {
        "grpc"
    }

    fn modes(&self) -> &'static [Mode] {
        &[Mode::Dispatcher]
    }

    fn dispatcher(&self) -> *const rapira_plugin {
        &raw const php::rapira_grpc_plugin
    }

    fn serve(self: Box<Self>, worker: Worker) -> ServeFuture {
        let Self { config, prepared } = *self;
        Box::pin(serve::serve(config, prepared, worker))
    }
}
