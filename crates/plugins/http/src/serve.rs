//! The accept loop, one hyper connection per socket, and the drain at the stop.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use rapira_net::{Connections, ListenAddr, Serve};
use rapira_sapi::Addr;
use rapira_sapi::plugin::Worker;
use rapira_sapi::work::Intake;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::bridge::{ConnectionState, Inflight};
use crate::handler::{Conn, Shared, respond};
use crate::{Config, Server};

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    shared: Arc<Shared>,
    connections: Connections,
    builder: http1::Builder,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.shared.inflight.cut();
    }
}

impl Serving {
    fn start(intake: Intake, config: Config) -> Self {
        let shared = Arc::new(Shared {
            cfg: config,
            intake,
            inflight: Arc::new(Inflight::default()),
            closing: AtomicBool::new(false),
        });
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(shared.cfg.keepalive_timeout);
        Self {
            shared,
            connections: Connections::default(),
            builder,
        }
    }

    /// Serves one connection and marks its state closed when the task ends.
    fn spawn_conn<S>(&self, stream: S, remote: Addr, server: Addr)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (closed_tx, closed_rx) = watch::channel(ConnectionState::default());
        let handler = Conn::new(Arc::clone(&self.shared), remote, server, closed_rx);
        let io = crate::bridge::TimedIo::new(
            TokioIo::new(stream),
            self.shared.cfg.write_timeout,
            closed_tx,
        );
        let chain = handler.chain();
        // BoxCloneService polls and calls through &mut, so each request takes its own clone.
        let service = hyper::service::service_fn(move |req| {
            let handler = Arc::clone(&handler);
            let chain = chain.clone();
            async move { Ok::<_, Infallible>(respond(&handler, chain, req).await) }
        });
        let connection = self.builder.serve_connection(io, service);
        self.connections.spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!(target: "http", "connection ended with error: {e}");
            }
        });
    }

    /// A held connection closes after its next response. An idle connection can stay open for the full grace period.
    async fn drain(self, grace: Duration) -> Result<()> {
        self.shared.closing.store(true, Ordering::Release);
        tracing::info!(
            target: "http",
            "draining {} open connections",
            self.connections.count()
        );
        self.connections
            .drain(grace, async {
                while self.shared.inflight.count() > 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
        let stranded = self.shared.inflight.count();
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
        // On macOS, getsockname fails with EINVAL on a socket that is shut down. https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/getsockname.2.html
        let server = match stream.local_addr() {
            Ok(server) => server,
            Err(e) => {
                tracing::debug!(target: "http", "connection from {peer} dropped: {e}");
                return;
            }
        };
        self.spawn_conn(stream, Addr::Inet(peer), Addr::Inet(server));
    }

    fn spawn_unix(&self, stream: tokio::net::UnixStream, peer: Option<&std::path::Path>) {
        let remote = Addr::Unix(peer.map(Into::into));
        self.spawn_conn(stream, remote, listen_addr(&self.shared.cfg.listen));
    }
}

pub(crate) async fn serve(server: Server, worker: Worker) -> Result<()> {
    let Server {
        config,
        prepared,
        spool: _spool,
    } = server;
    let serving = Serving::start(worker.intake, config);
    prepared.run(worker.stop, &serving).await?;
    serving.drain(worker.drain_grace).await
}

fn listen_addr(listen: &ListenAddr) -> Addr {
    match listen {
        ListenAddr::Tcp(a) => Addr::Inet(*a),
        ListenAddr::Unix(p) => Addr::Unix(Some(p.clone())),
    }
}
