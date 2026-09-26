use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use rapira_net::{Acceptor, ListenAddr, PreparedListener, Serve};
use rapira_sapi::Addr;
use rapira_sapi::plugin::Worker;
use rapira_sapi::work::Intake;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch::{Sender, channel};

use crate::bridge::ConnectionState;
use crate::handler::{Conn, Shared, respond};
use crate::{Config, Exchange, multipart};

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    shared: Arc<Shared>,
    graceful: GracefulShutdown,
    builder: http1::Builder,
}

impl Serving {
    fn start(
        intake: Intake<Exchange>,
        uploads: Option<Arc<multipart::Limits>>,
        config: Config,
    ) -> Self {
        match &config.listen {
            ListenAddr::Tcp(a) => tracing::info!(target: "http", "listening on http://{a}"),
            unix => tracing::info!(target: "http", "listening on {unix}"),
        }
        let shared = Arc::new(Shared {
            cfg: config,
            intake,
            uploads,
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
        let (closed_tx, closed_rx) = channel(ConnectionState::default());
        let handler = Conn::new(Arc::clone(&self.shared), remote, server, closed_rx);
        let io = crate::bridge::TimedIo::new(
            TokioIo::new(stream),
            self.shared.cfg.write_timeout,
            closed_tx.clone(),
        );
        spawn_connection(&self.builder, &self.graceful, io, handler, closed_tx);
    }

    /// Waits out the connections in flight. The acceptor is already gone.
    async fn drain(self, fatal: Option<anyhow::Error>, grace: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + grace;
        if tokio::time::timeout_at(deadline, self.graceful.shutdown())
            .await
            .is_err()
        {
            tracing::warn!(
                target: "http",
                "graceful connection shutdown did not finish within {grace:?}"
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
                "http drain timed out after {grace:?} with {stranded} request(s) in flight; \
                 their responses were cut short"
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

/// Serves one connection under `graceful` on its own task and marks `closed_tx` closed when it ends.
pub(crate) fn spawn_connection<I>(
    builder: &http1::Builder,
    graceful: &GracefulShutdown,
    io: I,
    handler: Arc<Conn>,
    closed_tx: Sender<ConnectionState>,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let chain = handler.chain();
    // `BoxCloneService` polls and calls through `&mut`, so each request takes its own clone.
    let service = hyper::service::service_fn(move |req| {
        let handler = Arc::clone(&handler);
        let chain = chain.clone();
        async move { Ok::<_, Infallible>(respond(handler, chain, req).await) }
    });
    spawn_watched(
        graceful.watch(builder.serve_connection(io, service)),
        closed_tx,
    );
}

fn spawn_watched(
    watched: impl Future<Output = hyper::Result<()>> + Send + 'static,
    closed_tx: Sender<ConnectionState>,
) {
    tokio::spawn(async move {
        if let Err(e) = watched.await {
            tracing::debug!(target: "http", "connection ended with error: {e}");
        }
        closed_tx.send_modify(|s| s.closed = true);
    });
}

/// Runs the accept loop on the calling thread until the stop flag, then drains the connections.
pub(crate) fn serve(
    intake: Intake<Exchange>,
    config: Config,
    prepared: PreparedListener,
    worker: Worker,
) -> Result<()> {
    let acceptor = Acceptor::adopt(prepared, worker.stop.clone(), &worker.handle)?;
    // The plugin parses multipart in dispatcher mode only: the other modes feed php-src's own rfc1867 through read_post.
    let dispatcher: bool = !config.superglobals;
    // Each worker spools in its own dir under the configured one.
    let spool_dir: Option<PathBuf> = match &config.uploads {
        Some(limits) if dispatcher => Some(multipart::create_worker_spool_dir(&limits.dir)?),
        _ => None,
    };
    let uploads = dispatcher.then(|| {
        let mut limits = config.uploads.clone().unwrap_or_default();
        if let Some(dir) = &spool_dir {
            limits.dir = dir.clone();
        }
        Arc::new(limits)
    });
    let serving = Serving::start(intake, uploads, config);
    let fatal = acceptor.run(&worker.handle, &serving);
    let drained = worker
        .handle
        .block_on(serving.drain(fatal, worker.drain_grace));
    // The dir goes when the plugin drain ends. A spooled file of an exchange that still runs past the drain is lost with it.
    if let Some(dir) = &spool_dir
        && let Err(e) = std::fs::remove_dir_all(dir)
    {
        tracing::warn!(target: "rapira", "removing spool dir {}: {e}", dir.display());
    }
    drained
}

fn listen_addr(listen: &ListenAddr) -> Addr {
    match listen {
        ListenAddr::Tcp(a) => Addr::Inet(*a),
        ListenAddr::Unix(p) => Addr::Unix(Some(p.clone())),
    }
}
