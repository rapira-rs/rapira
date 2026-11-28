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
use crate::dispatch::{InFlight, PhpDispatcher};

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    service: ConnectRpcService<Chain<Router, PhpDispatcher>>,
    connection: ConnectionConfig,
    health: watch::Sender<Status>,
    connections: Connections,
    /// The calls whose response is not complete: a unary call until its response is ready, and a stream of PHP until its body ends.
    inflight: Arc<AtomicUsize>,
}

impl Serving {
    fn spawn<I>(&self, io: I, info: ConnectionInfo)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut stop = self.health.subscribe();
        let draining = stop.clone();
        // Trigger hyper's graceful_shutdown only for HTTP/2 (whose first server write starts with the 0 byte of the initial SETTINGS frame length: https://www.rfc-editor.org/rfc/rfc9113#section-3.4).
        // HTTP/1.1 keep-alive connections stay open to serve one final request with `Connection: close` (lines 63-68).
        let h2 = Arc::new(Notify::new());
        let mut first = Some(Arc::clone(&h2));
        let io = InspectWriter::new(io, move |bytes| {
            if let Some(h2) = first.take()
                && bytes.first() == Some(&0)
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

    /// Waits for the open connections after the listener stops or fails.
    async fn drain(self, run: Result<()>, grace: Duration) -> Result<()> {
        self.health.send_replace(Status::NotServing);
        drop(self.health);
        tracing::info!(target: "grpc", "draining open connections");
        let drained = self.connections.drain(grace).await;
        // An HTTP/1.1 connection that stays idle until the deadline holds no call, so only a call in flight fails the drain.
        let cut = self.inflight.load(Relaxed);
        let drain_err = (!drained && cut > 0).then(|| {
            anyhow!("grpc drain timed out after {grace:?}; {cut} call(s) in flight were cut")
        });
        if let Err(err) = run {
            if let Some(drain_err) = drain_err {
                tracing::warn!(target: "grpc", "{drain_err}");
            }
            return Err(err);
        }
        if let Some(drain_err) = drain_err {
            return Err(drain_err);
        }
        tracing::info!(target: "grpc", "drained cleanly; accept loop stopped");
        Ok(())
    }
}

impl Serve for Serving {
    fn spawn_tcp(&self, stream: std::net::TcpStream, peer: std::net::SocketAddr) {
        match tokio::net::TcpStream::from_std(stream) {
            Ok(stream) => {
                let mut info = ConnectionInfo::new().with_peer_addr(peer);
                info.extensions_mut().insert(Addr::Inet(peer));
                self.spawn(stream, info);
            }
            Err(e) => {
                tracing::debug!(target: "grpc", "registering tcp connection from {peer}: {e}");
            }
        }
    }

    fn spawn_unix(&self, stream: std::os::unix::net::UnixStream, peer: Option<std::path::PathBuf>) {
        match tokio::net::UnixStream::from_std(stream) {
            Ok(stream) => {
                let mut info = ConnectionInfo::new();
                info.extensions_mut().insert(Addr::Unix(peer));
                self.spawn(stream, info);
            }
            Err(e) => tracing::debug!(target: "grpc", "registering unix connection: {e}"),
        }
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
    let run = server.listener.run(stop, &serving).await;
    serving.drain(run, drain_grace).await
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use rapira_sapi::queue::PoolQueue;

    use super::*;
    use crate::schema::{Schema, Source};

    fn test_serving() -> (Serving, watch::Receiver<Status>, Arc<AtomicUsize>) {
        let (_queue, intake) = PoolQueue::new();
        let schema_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/grpc/echo.binpb");
        let schema = Arc::new(Schema::load(&Source::File(schema_path), None).expect("echo schema"));
        let inflight = Arc::new(AtomicUsize::new(0));
        let dispatcher = PhpDispatcher {
            schema,
            intake,
            inflight: Arc::clone(&inflight),
        };
        let service = ConnectRpcService::new(Chain(Router::new(), dispatcher));
        let (health_tx, health_rx) = watch::channel(Status::Serving);
        let serving = Serving {
            service,
            connection: ConnectionConfig::default(),
            health: health_tx,
            connections: Connections::default(),
            inflight: Arc::clone(&inflight),
        };
        (serving, health_rx, inflight)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_runs_after_listener_error_and_preserves_primary_error() {
        struct Case {
            name: &'static str,
            run_err: Option<&'static str>,
            connection_finishes: bool,
            want_err_contains: Option<&'static str>,
        }
        let cases = [
            Case {
                name: "clean stop drains active connection and returns Ok",
                run_err: None,
                connection_finishes: true,
                want_err_contains: None,
            },
            Case {
                name: "listener error drains active connection before returning listener error",
                run_err: Some("listener failed"),
                connection_finishes: true,
                want_err_contains: Some("listener failed"),
            },
            Case {
                name: "listener error with stuck inflight call drains for grace and returns listener error",
                run_err: Some("listener failed"),
                connection_finishes: false,
                want_err_contains: Some("listener failed"),
            },
            Case {
                name: "clean stop with stuck inflight call returns drain timeout error",
                run_err: None,
                connection_finishes: false,
                want_err_contains: Some("grpc drain timed out"),
            },
        ];

        for case in cases {
            let (serving, mut health_rx, inflight) = test_serving();
            let finished = Arc::new(AtomicBool::new(false));
            let finished_clone = Arc::clone(&finished);
            let inflight_clone = Arc::clone(&inflight);
            let mut conn_health = health_rx.clone();
            let finishes = case.connection_finishes;
            serving.connections.spawn(async move {
                let _guard = InFlight::start(&inflight_clone);
                if finishes {
                    let _ = conn_health.wait_for(|&s| s == Status::NotServing).await;
                    finished_clone.store(true, Relaxed);
                } else {
                    std::future::pending::<()>().await;
                }
            });
            tokio::task::yield_now().await;

            let run = match case.run_err {
                Some(msg) => Err(anyhow!(msg)),
                None => Ok(()),
            };
            let got = serving.drain(run, Duration::from_millis(30)).await;
            assert_eq!(
                *health_rx.borrow_and_update(),
                Status::NotServing,
                "{}",
                case.name
            );
            assert_eq!(
                finished.load(Relaxed),
                case.connection_finishes,
                "{}",
                case.name
            );
            match case.want_err_contains {
                None => assert!(got.is_ok(), "{}: got={got:?}", case.name),
                Some(needle) => {
                    let msg = format!("{:#}", got.expect_err(case.name));
                    assert!(msg.contains(needle), "{}: msg={msg}", case.name);
                }
            }
        }
    }
}
