use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use extension_api::{Extension, ListenAddr, Middleware, Php, PrepareCtx, PreparedListener, Result};
use rapira_net::{Stop, join_thread};
use tokio::runtime::Builder;

mod bridge;
mod check;
mod handler;
mod request;
mod response;
mod serve;

#[derive(Clone)]
pub struct Config {
    pub listen: ListenAddr,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    pub unsafe_field_names: UnsafeFieldNames,
    pub superglobals: bool,
    pub write_timeout: Duration,
    pub drain_grace: Duration,
    pub keepalive_timeout: Duration,
    pub middleware: Vec<Arc<dyn Middleware>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeFieldNames {
    Drop,
    Reject,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ListenAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 8000))),
            server_name: "localhost".to_owned(),
            server_port: 8000,
            max_body_size: 8 * 1024 * 1024,
            unsafe_field_names: UnsafeFieldNames::Drop,
            superglobals: true,
            write_timeout: Duration::from_secs(30),
            drain_grace: Duration::from_secs(25),
            keepalive_timeout: Duration::from_secs(60),
            middleware: Vec::new(),
        }
    }
}

pub struct Server {
    config: Config,
    prepared: Option<PreparedListener>,
    stop: Option<Stop>,
    join: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Extension for Server {
    type Config = Config;

    fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
            stop: None,
            join: None,
        }
    }

    fn name(&self) -> &str {
        "rapira-http"
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
        let prepared = match &self.config.listen {
            ListenAddr::Tcp(addr) => ctx.bind_tcp(*addr)?,
            ListenAddr::Unix(path) => ctx.bind_unix(path)?,
        };
        match prepared.addr() {
            ListenAddr::Tcp(a) => tracing::info!(target: "http", "prepared listener on {a}"),
            ListenAddr::Unix(p) => {
                tracing::info!(target: "http", "prepared listener on {}", p.display());
            }
        }
        self.prepared = Some(prepared);
        Ok(())
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        let config = self.config.clone();
        let Some(prepared) = self.prepared.take() else {
            return Err(anyhow!("http listener was not prepared"));
        };
        let stop = Stop::new().map_err(|e| anyhow!("creating the http stop handle: {e}"))?;
        let handle = stop.handle();

        let thread = std::thread::Builder::new()
            .name("rapira-http".into())
            .spawn(move || {
                let rt = Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("rapira-http-io")
                    .build()
                    .map_err(|e| anyhow!("building the http runtime: {e}"))?;
                serve::serve(php, config, prepared, handle, &rt)
            })?;

        self.stop = Some(stop);
        let join = self.join.insert(tokio::task::spawn_blocking(move || {
            join_thread(thread, "http")
        }));
        let result = join.await;
        self.join = None;
        result.map_err(|e| anyhow!("http join task failed: {e}"))?
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(stop) = self.stop.take() {
            stop.stop();
        }
        if let Some(join) = self.join.take() {
            join.await
                .map_err(|e| anyhow!("http join task failed: {e}"))??;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future as _;
    use std::pin::Pin;
    use std::task::Poll;

    use extension_api::{Backend, Reply, Request};

    use super::*;

    struct UnusedBackend;

    impl Backend for UnusedBackend {
        fn exec(&self, _req: Request) -> Pin<Box<dyn Future<Output = Result<Reply>> + Send + '_>> {
            unreachable!("the lifecycle test does not send a request")
        }
    }

    #[tokio::test]
    async fn shutdown_joins_the_server_after_run_is_cancelled() {
        let mut server = Server::init(Config {
            listen: ListenAddr::Tcp(([127, 0, 0, 1], 0).into()),
            ..Config::default()
        });
        let mut ctx = PrepareCtx::new();
        server.prepare(&mut ctx).unwrap();
        let backend = Arc::new(UnusedBackend);
        let php = Php::new(backend.clone());

        let mut run = Box::pin(server.run(php));
        assert!(std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx).is_pending())).await);
        drop(run);

        assert!(server.join.is_some());
        server.shutdown().await.unwrap();
        assert!(server.join.is_none());
        assert_eq!(Arc::strong_count(&backend), 1);
    }
}
