use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use connectrpc::Router;
use connectrpc_health::StaticChecker;
use connectrpc_reflection::Reflector;
use rapira_net::{ListenAddr, PrepareCtx, PreparedListener};
use rapira_sapi::plugin::{Mode, PhpPart, Plugin, Worker};
use rapira_sapi::work::Intake;

mod call;
mod dispatch;
mod php;
mod schema;
mod serve;

pub use call::{Call, RpcProtocol, RpcStatus, UnaryCall, UnaryReply};
pub use php::{DISPATCHER_CLASSES, PHP_PART};
pub use schema::{MethodInfo, Schema, ServiceInfo, set_services};

#[derive(Clone)]
pub struct Config {
    pub listen: ListenAddr,
    pub schema: Arc<Schema>,
    /// Serve `grpc.reflection.v1` and `v1alpha` for the configured services.
    pub reflection: bool,
    /// The timeout of a call whose client sets no deadline.
    pub default_timeout: Option<Duration>,
    /// The longest timeout that a client can set.
    pub max_timeout: Option<Duration>,
    pub drain_grace: Duration,
    /// HTTP/2 PING cadence and the wait for its ACK. A peer that is gone without a FIN sends no ACK, so its connection closes within the sum and does not hold a later drain.
    pub keepalive_interval: Duration,
    pub keepalive_timeout: Duration,
}

pub struct Server {
    config: Config,
    prepared: Option<Prepared>,
    /// The intake a test injects. None: the worker's sink.
    intake: Option<Intake<Call>>,
}

/// What `prepare` leaves for `serve`: the listener and the routes that the plugin answers without PHP.
pub(crate) struct Prepared {
    pub(crate) listener: PreparedListener,
    pub(crate) router: Router,
    pub(crate) health: Arc<StaticChecker>,
}

impl Server {
    pub fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
            intake: None,
        }
    }

    /// A server that submits to `intake` in place of the worker's sink.
    pub fn with_intake(config: Config, intake: Intake<Call>) -> Self {
        Self {
            intake: Some(intake),
            ..Self::init(config)
        }
    }
}

impl Plugin for Server {
    fn name(&self) -> &'static str {
        "grpc"
    }

    fn modes(&self) -> &'static [Mode] {
        &[Mode::Dispatcher]
    }

    fn php(&self) -> Option<PhpPart> {
        Some(PHP_PART)
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
        set_services(self.config.schema.services().to_vec())?;
        let listener = ctx.bind(&self.config.listen)?;
        tracing::info!(target: "grpc", "prepared listener on {}", listener.addr());

        let names: Vec<String> = self
            .config
            .schema
            .services()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let (mut router, health) = connectrpc_health::install_static(Router::new(), names.clone());
        if self.config.reflection {
            let reflector = Reflector::from_descriptor_pool(Arc::clone(self.config.schema.pool()))
                .map_err(|e| anyhow!("building grpc reflection: {e}"))?
                .with_services(names);
            router = connectrpc_reflection::install(router, reflector);
        }
        self.prepared = Some(Prepared {
            listener,
            router,
            health,
        });
        Ok(())
    }

    fn serve(self: Box<Self>, worker: Worker) -> Result<()> {
        let Self {
            config,
            prepared,
            intake,
        } = *self;
        let Some(prepared) = prepared else {
            return Err(anyhow!("grpc listener was not prepared"));
        };
        let intake = intake.unwrap_or_else(|| Intake::new(worker.sink.clone()));
        serve::serve(intake, config, prepared, worker)
    }
}
