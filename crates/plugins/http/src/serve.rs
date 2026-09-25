use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use extension_api::{Addr, ListenAddr, Php, PreparedListener, Result};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use rapira_net::{Acceptor, Serve, StopHandle};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch::channel;

use crate::Config;
use crate::handler::{RapiraService, Shared};

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    shared: Arc<Shared>,
    graceful: GracefulShutdown,
    builder: http1::Builder,
}

impl Serving {
    fn start(php: Php, config: Config) -> Self {
        match &config.listen {
            ListenAddr::Tcp(a) => tracing::info!(target: "http", "listening on http://{a}"),
            unix => tracing::info!(target: "http", "listening on {unix}"),
        }
        let chain: Arc<[_]> = config.middleware.clone().into();
        let shared = Arc::new(Shared {
            cfg: config,
            php,
            chain,
            inflight: Arc::new(AtomicUsize::new(0)),
        });
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(shared.cfg.keepalive_timeout);
        Self {
            shared,
            graceful: GracefulShutdown::new(),
            builder,
        }
    }

    fn spawn_conn<S>(&self, stream: S, remote: Addr, server: Addr)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (closed_tx, closed_rx) = channel(crate::bridge::ConnectionState::default());
        let svc = RapiraService::new(Arc::clone(&self.shared), remote, server, closed_rx);
        let io = crate::bridge::TimedIo::new(
            TokioIo::new(stream),
            self.shared.cfg.write_timeout,
            closed_tx.clone(),
        );
        let connection = self.builder.serve_connection(io, svc);
        let watched = self.graceful.watch(connection);
        tokio::spawn(async move {
            if let Err(e) = watched.await {
                tracing::debug!(target: "http", "connection ended with error: {e}");
            }
            closed_tx.send_modify(|s| s.closed = true);
        });
    }

    /// Waits out the connections in flight. The acceptor is already gone.
    async fn drain(self, fatal: Option<anyhow::Error>) -> Result<()> {
        let deadline = tokio::time::Instant::now() + self.shared.cfg.drain_grace;
        if tokio::time::timeout_at(deadline, self.graceful.shutdown())
            .await
            .is_err()
        {
            tracing::warn!(
                target: "http",
                "graceful connection shutdown did not finish within {:?}",
                self.shared.cfg.drain_grace
            );
        }
        while self.shared.inflight.load(Ordering::Acquire) > 0
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let stranded = self.shared.inflight.load(Ordering::Acquire);
        if let Some(e) = fatal {
            if stranded > 0 {
                tracing::warn!(
                    target: "http",
                    "{stranded} request(s) still in flight when the listener failed"
                );
            }
            return Err(e);
        }
        if stranded > 0 {
            return Err(anyhow!(
                "http drain timed out after {:?} with {stranded} request(s) in flight; \
                 their responses were cut short",
                self.shared.cfg.drain_grace
            ));
        }
        tracing::info!(target: "http", "drained cleanly; accept loop stopped");
        Ok(())
    }
}

impl Serve for Serving {
    fn spawn_tcp(&self, stream: tokio::net::TcpStream, peer: std::net::SocketAddr) {
        let server = stream
            .local_addr()
            .map(Addr::Inet)
            .unwrap_or_else(|_| listen_addr(&self.shared.cfg.listen));
        self.spawn_conn(stream, Addr::Inet(peer), server);
    }

    fn spawn_unix(&self, stream: tokio::net::UnixStream, peer: Option<&std::path::Path>) {
        let remote = Addr::Unix(peer.map(Into::into));
        self.spawn_conn(stream, remote, listen_addr(&self.shared.cfg.listen));
    }
}

/// Runs the accept loop on the calling thread, then drains the connections.
pub(crate) fn serve(
    php: Php,
    config: Config,
    prepared: PreparedListener,
    stop: StopHandle,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let acceptor = Acceptor::adopt(prepared, stop, rt)?;
    let serving = Serving::start(php, config);
    let fatal = acceptor.run(rt, &serving);
    rt.block_on(serving.drain(fatal))
}

fn listen_addr(listen: &ListenAddr) -> Addr {
    match listen {
        ListenAddr::Tcp(a) => Addr::Inet(*a),
        ListenAddr::Unix(p) => Addr::Unix(Some(p.clone())),
    }
}
