use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use connectrpc::server::serve_connection;
use connectrpc::{
    Chain, CompressionRegistry, ConnectRpcService, ConnectionConfig, ConnectionInfo,
    DeadlinePolicy, GzipProvider, Router,
};
use connectrpc_health::StaticChecker;
use extension_api::{Addr, Php, Result};
use rapira_net::{Acceptor, Serve, StopHandle};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::dispatch::PhpDispatcher;
use crate::{Config, Prepared};

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    service: ConnectRpcService<Chain<Router, PhpDispatcher>>,
    connection: ConnectionConfig,
    health: Arc<StaticChecker>,
    /// Each connection holds a receiver until it ends, so `closed()` resolves when the last connection is gone.
    shutdown: watch::Sender<bool>,
}

impl Serving {
    fn start(php: Php, config: &Config, router: Router, health: Arc<StaticChecker>) -> Self {
        tracing::info!(target: "grpc", "listening on {}", config.listen);
        let mut deadlines = DeadlinePolicy::new();
        if let Some(timeout) = config.default_timeout {
            deadlines = deadlines.with_default_timeout(timeout);
        }
        if let Some(max) = config.max_timeout {
            deadlines = deadlines.with_max(max);
        }
        let dispatcher = PhpDispatcher {
            schema: Arc::clone(&config.schema),
            php,
        };
        // The host routes come first, so a configured service cannot hide health or reflection.
        let service = ConnectRpcService::new(Chain(router, dispatcher))
            .with_deadline_policy(deadlines)
            // The default registry offers every codec that the build compiles, zstd included.
            .with_compression(CompressionRegistry::new().register(GzipProvider::default()));
        Self {
            service,
            connection: ConnectionConfig::new()
                .with_http2_keepalive_interval(config.keepalive_interval)
                .with_http2_keepalive_timeout(config.keepalive_timeout),
            health,
            shutdown: watch::Sender::new(false),
        }
    }

    fn spawn<I>(&self, io: I, info: ConnectionInfo)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let open = self.shutdown.subscribe();
        let mut stop = open.clone();
        let connection = serve_connection(
            io,
            info,
            self.service.clone(),
            self.connection.clone(),
            async move {
                let _ = stop.wait_for(|stop| *stop).await;
            },
        );
        tokio::spawn(async move {
            // The shutdown future drops its receiver when shutdown starts, and the calls in flight continue after that. `open` keeps `closed()` pending until the connection ends.
            let closed = connection.await;
            drop(open);
            tracing::debug!(target: "grpc", "connection closed: {:?}", closed.reason());
        });
    }

    /// Waits out the connections in flight. The acceptor is already gone.
    async fn drain(self, fatal: Option<anyhow::Error>, grace: Duration) -> Result<()> {
        // `StaticChecker::shutdown` only sets NOT_SERVING. A `Watch` stream ends only when its service is removed, and an open stream holds its connection until the grace ends.
        self.health.shutdown();
        for name in self.health.services() {
            self.health.remove_service(&name);
        }
        self.shutdown.send_replace(true);
        let drained = tokio::time::timeout(grace, self.shutdown.closed())
            .await
            .is_ok();
        if let Some(e) = fatal {
            return Err(e);
        }
        if !drained {
            return Err(anyhow!(
                "grpc drain timed out after {grace:?}; open connections were cut"
            ));
        }
        tracing::info!(target: "grpc", "drained cleanly; accept loop stopped");
        Ok(())
    }
}

impl Serve for Serving {
    fn spawn_tcp(&self, stream: tokio::net::TcpStream, peer: std::net::SocketAddr) {
        let mut info = ConnectionInfo::new().with_peer_addr(peer);
        info.extensions_mut().insert(Addr::Inet(peer));
        self.spawn(stream, info);
    }

    fn spawn_unix(&self, stream: tokio::net::UnixStream, peer: Option<&std::path::Path>) {
        let mut info = ConnectionInfo::new();
        info.extensions_mut()
            .insert(Addr::Unix(peer.map(Into::into)));
        self.spawn(stream, info);
    }
}

/// Runs the accept loop on the calling thread, then drains the connections.
pub(crate) fn serve(
    php: Php,
    config: Config,
    prepared: Prepared,
    stop: StopHandle,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let acceptor = Acceptor::adopt(prepared.listener, stop, rt)?;
    let serving = Serving::start(php, &config, prepared.router, prepared.health);
    let fatal = acceptor.run(rt, &serving);
    rt.block_on(serving.drain(fatal, config.drain_grace))
}
