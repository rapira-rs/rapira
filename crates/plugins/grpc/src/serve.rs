//! The accept loop, one connectrpc connection per socket, and the drain at the stop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use anyhow::{Result, anyhow};
use connectrpc::server::serve_connection;
use connectrpc::{
    Chain, CompressionRegistry, ConnectRpcBody, ConnectRpcService, ConnectionConfig,
    ConnectionInfo, DeadlinePolicy, GzipProvider, Router, http,
};
use connectrpc_health::StaticChecker;
use rapira_net::Serve;
use rapira_sapi::Addr;
use rapira_sapi::plugin::Worker;
use rapira_sapi::work::Intake;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tower::ServiceExt as _;

use crate::dispatch::PhpDispatcher;
use crate::{Config, Prepared};

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    service: ConnectRpcService<Chain<Router, PhpDispatcher>>,
    connection: ConnectionConfig,
    health: Arc<StaticChecker>,
    shutdown: watch::Sender<bool>,
    /// Each connection holds a receiver until it ends. Dropping the sender cuts the remaining connections.
    connections: watch::Sender<()>,
    /// The calls whose response is not complete: a unary call until its response is ready, and a stream of PHP until its body ends.
    inflight: Arc<AtomicUsize>,
}

/// Counts one call in flight until it drops, also when the client leaves first.
pub(crate) struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    pub(crate) fn start(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Relaxed);
        Self(Arc::clone(count))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

impl Serving {
    fn start(intake: Intake, config: &Config, router: Router, health: Arc<StaticChecker>) -> Self {
        let mut deadlines = DeadlinePolicy::new();
        if let Some(timeout) = config.default_timeout {
            deadlines = deadlines.with_default_timeout(timeout);
        }
        if let Some(max) = config.max_timeout {
            deadlines = deadlines.with_max(max);
        }
        let inflight = Arc::new(AtomicUsize::new(0));
        let dispatcher = PhpDispatcher {
            schema: Arc::clone(&config.schema),
            intake,
            inflight: Arc::clone(&inflight),
        };
        // The plugin routes come first, so a configured service cannot hide health or reflection.
        let mut service = ConnectRpcService::new(Chain(router, dispatcher))
            .with_deadline_policy(deadlines)
            // The default registry offers every codec that the build compiles, zstd included.
            // The default policy compresses a reply of 1 KiB or more for a client that accepts gzip. The dispatcher turns it off per reply.
            .with_compression(CompressionRegistry::new().register(GzipProvider::default()));
        // Interceptors cover GET and POST requests to every route: PHP, health, reflection and unknown paths.
        for interceptor in &config.interceptors {
            service = service.with_interceptor_arc(Arc::clone(interceptor));
        }
        Self {
            service,
            connection: ConnectionConfig::new()
                .with_http2_keepalive_interval(config.keepalive_interval)
                .with_http2_keepalive_timeout(config.keepalive_timeout),
            health,
            shutdown: watch::Sender::new(false),
            connections: watch::Sender::new(()),
            inflight,
        }
    }

    fn spawn<I>(&self, io: I, info: ConnectionInfo)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut stop = self.shutdown.subscribe();
        let draining = stop.clone();
        // During the drain an HTTP/1.1 connection closes itself after its next response, and h2 gets GOAWAY.
        let http1 = Arc::new(AtomicBool::new(false));
        let (mark, close) = (Arc::clone(&http1), Arc::clone(&http1));
        let inflight = Arc::clone(&self.inflight);
        let service = self
            .service
            .clone()
            .map_request(move |req: http::Request<_>| {
                mark.store(req.version() < http::Version::HTTP_2, Relaxed);
                req
            })
            .map_future(move |call| {
                let counted = InFlight::start(&inflight);
                async move {
                    let response = call.await;
                    drop(counted);
                    response
                }
            })
            .map_response(move |response| {
                let mut response = status_in_trailers(response);
                if close.load(Relaxed) && *draining.borrow() {
                    response.headers_mut().insert(
                        http::header::CONNECTION,
                        http::HeaderValue::from_static("close"),
                    );
                }
                response
            });
        let connection = serve_connection(io, info, service, self.connection.clone(), async move {
            let _ = stop.wait_for(|stop| *stop).await;
            if http1.load(Relaxed) {
                // The connection closes after its next response, or the drain deadline cuts it.
                std::future::pending::<()>().await;
            }
        });
        let mut cut = self.connections.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = cut.changed() => {}
                closed = connection => {
                    tracing::debug!(target: "grpc", "connection closed: {:?}", closed.reason());
                }
            }
        });
    }

    /// Waits for the open connections after the listener closes.
    async fn drain(self, grace: Duration) -> Result<()> {
        // StaticChecker::shutdown only sets NOT_SERVING. A Watch stream ends only when its service is removed, and an open stream holds its connection until the grace ends.
        self.health.shutdown();
        for name in self.health.services() {
            self.health.remove_service(&name);
        }
        self.shutdown.send_replace(true);
        tracing::info!(target: "grpc", "draining open connections");
        let drained = tokio::time::timeout(grace, self.connections.closed())
            .await
            .is_ok();
        // An HTTP/1.1 connection that stays idle until the deadline holds no call, so only a call in flight fails the drain.
        let cut = self.inflight.load(Relaxed);
        if !drained && cut > 0 {
            return Err(anyhow!(
                "grpc drain timed out after {grace:?}; {cut} call(s) in flight were cut"
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

/// A gRPC error carries its whole status in the trailers. connectrpc also copies the code and the message into the head. A client that takes the status from the head, such as tonic, then drops the details and the trailers. See https://github.com/connectrpc/connect-rust/issues/286. Only that copy puts these fields in a head, because PHP cannot set a `grpc-` field.
fn status_in_trailers(
    mut response: http::Response<ConnectRpcBody>,
) -> http::Response<ConnectRpcBody> {
    let headers = response.headers_mut();
    headers.remove("grpc-status");
    headers.remove("grpc-message");
    response
}

pub(crate) async fn serve(config: Config, prepared: Prepared, worker: Worker) -> Result<()> {
    let serving = Serving::start(worker.intake, &config, prepared.router, prepared.health);
    prepared.listener.run(worker.stop, &serving).await?;
    serving.drain(worker.drain_grace).await
}
