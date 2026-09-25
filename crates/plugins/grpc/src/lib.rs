use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use connectrpc::Router;
use connectrpc_health::StaticChecker;
use connectrpc_reflection::Reflector;
use extension_api::{Extension, ListenAddr, Php, PrepareCtx, PreparedListener, Result};
use rapira_net::ServerThread;

mod dispatch;
mod schema;
mod serve;

pub use schema::{MethodInfo, Schema, ServiceInfo};

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
    thread: ServerThread,
}

/// What `prepare` leaves for `run`: the listener and the routes that the host answers without PHP.
pub(crate) struct Prepared {
    pub(crate) listener: PreparedListener,
    pub(crate) router: Router,
    pub(crate) health: Arc<StaticChecker>,
}

impl Extension for Server {
    type Config = Config;

    fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
            thread: ServerThread::default(),
        }
    }

    fn name(&self) -> &str {
        "rapira-grpc"
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
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

    async fn run(&mut self, php: Php) -> Result<()> {
        let config = self.config.clone();
        let Some(prepared) = self.prepared.take() else {
            return Err(anyhow!("grpc listener was not prepared"));
        };
        self.thread
            .run("grpc", move |stop, rt| {
                serve::serve(php, config, prepared, stop, rt)
            })
            .await
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.thread.shutdown().await
    }
}
