use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::anyhow;
use extension_api::{Extension, ListenAddr, Middleware, Php, PrepareCtx, PreparedListener, Result};
use tokio::runtime::Builder;
use tokio::sync::watch;

mod codec;
mod deadline;
mod handler;
mod metadata;
mod reflection;
mod registry;
mod response;
mod schema;
mod serve;

pub use registry::Registry;

#[derive(Clone)]
pub struct Config {
    pub listen: ListenAddr,
    pub registry: Arc<Registry>,
    pub reflection: bool,
    pub interceptors: Vec<Arc<dyn Middleware>>,
    pub gzip_responses: bool,
    pub max_request_message_size: usize,
    pub max_response_message_size: usize,
    pub drain_grace: Duration,
}

pub struct Server {
    config: Config,
    prepared: Option<PreparedListener>,
    shutdown: Option<watch::Sender<bool>>,
    join: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Extension for Server {
    type Config = Config;

    fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
            shutdown: None,
            join: None,
        }
    }
    fn name(&self) -> &str {
        "rapira-grpc"
    }
    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
        let prepared = match &self.config.listen {
            ListenAddr::Tcp(addr) => ctx.bind_tcp(*addr)?,
            ListenAddr::Unix(path) => ctx.bind_unix(path)?,
        };
        tracing::info!(target: "grpc", "prepared listener on {:?}", prepared.addr());
        self.prepared = Some(prepared);
        Ok(())
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = self.config.clone();
        let Some(prepared) = self.prepared.take() else {
            return Err(anyhow!("grpc listener was not prepared"));
        };
        let thread = std::thread::Builder::new()
            .name("rapira-grpc".into())
            .spawn(move || {
                let rt = Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("rapira-grpc-io")
                    .build()
                    .map_err(|e| anyhow!("building the grpc runtime: {e}"))?;
                rt.block_on(serve::serve(php, config, prepared, shutdown_rx))
            })?;
        self.shutdown = Some(shutdown_tx);
        let join = self
            .join
            .insert(tokio::task::spawn_blocking(move || join_thread(thread)));
        let result = join.await;
        self.join = None;
        result.map_err(|e| anyhow!("grpc join task failed: {e}"))?
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(true);
        }
        if let Some(join) = self.join.take() {
            join.await
                .map_err(|e| anyhow!("grpc join task failed: {e}"))??;
        }
        Ok(())
    }
}

fn join_thread(thread: JoinHandle<Result<()>>) -> Result<()> {
    thread.join().map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown panic");
        anyhow!("grpc server thread panicked: {message}")
    })?
}

#[cfg(test)]
mod tests;
