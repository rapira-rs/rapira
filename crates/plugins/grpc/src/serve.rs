//! The accept loop, one connectrpc connection per socket, and the drain at the stop.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use anyhow::{Result, anyhow};
use connectrpc::server::serve_connection;
use connectrpc::{
    Chain, CompressionRegistry, ConnectRpcBody, ConnectRpcService, ConnectionConfig,
    ConnectionInfo, GzipProvider, Router, http,
};
use connectrpc_health::Status;
use rapira_net::{Connections, Serve};
use rapira_sapi::Addr;
use rapira_sapi::work::Intake;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, watch};
use tokio_util::io::InspectWriter;
use tower::Service as _;

use crate::Server;
use crate::dispatch::PhpDispatcher;

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    service: ConnectRpcService<Chain<Router, PhpDispatcher>>,
    connection: ConnectionConfig,
    health: watch::Sender<Status>,
    connections: Connections,
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
    fn spawn<I>(&self, io: I, info: ConnectionInfo)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut stop = self.health.subscribe();
        let draining = stop.clone();
        // The server's first write starts with 'H' for HTTP/1 or the length of its initial SETTINGS frame for HTTP/2. https://www.rfc-editor.org/rfc/rfc9113#section-3.4
        let h2 = Arc::new(Notify::new());
        let mut first = Some(Arc::clone(&h2));
        let io = InspectWriter::new(io, move |bytes| {
            if let Some(h2) = first.take()
                && bytes[0] == 0
            {
                h2.notify_one();
            }
        });
        let inflight = Arc::clone(&self.inflight);
        let mut service = self.service.clone();
        let service = tower::service_fn(move |req: http::Request<_>| {
            let http1 = req.version() < http::Version::HTTP_2;
            let draining = draining.clone();
            let counted = InFlight::start(&inflight);
            let call = service.call(req);
            async move {
                let mut response = status_in_trailers(call.await?);
                if http1 && *draining.borrow() == Status::NotServing {
                    response.headers_mut().insert(
                        http::header::CONNECTION,
                        http::HeaderValue::from_static("close"),
                    );
                }
                drop(counted);
                Ok::<_, Infallible>(response)
            }
        });
        let connection = serve_connection(io, info, service, self.connection.clone(), async move {
            let _ = stop.changed().await;
            h2.notified().await;
        });
        self.connections.spawn(async move {
            let closed = connection.await;
            tracing::debug!(target: "grpc", "connection closed: {:?}", closed.reason());
        });
    }

    /// Waits for the open connections after the listener closes.
    async fn drain(self, grace: Duration) -> Result<()> {
        self.health.send_replace(Status::NotServing);
        drop(self.health);
        tracing::info!(target: "grpc", "draining open connections");
        let drained = self.connections.drain(grace, std::future::ready(())).await;
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

pub(crate) async fn serve(
    server: Server,
    intake: Intake,
    stop: watch::Receiver<bool>,
    drain_grace: Duration,
) -> Result<()> {
    let inflight = Arc::new(AtomicUsize::new(0));
    let dispatcher = PhpDispatcher {
        schema: server.schema,
        intake,
        inflight: Arc::clone(&inflight),
    };
    // The plugin routes come first, so a configured service cannot hide health or reflection.
    let mut service = ConnectRpcService::new(Chain(server.router, dispatcher))
        .with_deadline_policy(server.deadlines)
        // The default registry offers every codec that the build compiles, zstd included.
        // The default policy compresses a reply of 1 KiB or more for a client that accepts gzip. The dispatcher turns it off per reply.
        .with_compression(CompressionRegistry::new().register(GzipProvider::default()));
    // Interceptors cover GET and POST requests to every route: PHP, health, reflection and unknown paths.
    for interceptor in server.interceptors {
        service = service.with_interceptor_arc(interceptor);
    }
    let serving = Serving {
        service,
        connection: server.connection,
        health: server.health,
        connections: Connections::default(),
        inflight,
    };
    server.listener.run(stop, &serving).await?;
    serving.drain(drain_grace).await
}
