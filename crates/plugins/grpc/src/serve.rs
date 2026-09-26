use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use connectrpc::server::serve_connection;
use connectrpc::{
    Chain, CompressionRegistry, ConnectRpcBody, ConnectRpcService, ConnectionConfig,
    ConnectionInfo, DeadlinePolicy, GzipProvider, Router,
};
use connectrpc_health::StaticChecker;
use rapira_net::{Acceptor, Serve, Stop};
use rapira_sapi::Addr;
use rapira_sapi::plugin::Worker;
use rapira_sapi::work::Intake;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tower::Layer;
use tower::util::MapResponse;

use crate::dispatch::PhpDispatcher;
use crate::interceptor::Service;
use crate::{Call, Config, Interceptor, Prepared};

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    service: ConnectRpcService<Chain<Router, PhpDispatcher>>,
    interceptors: Vec<Interceptor>,
    connection: ConnectionConfig,
    health: Arc<StaticChecker>,
    /// Each connection holds a receiver until it ends, so `closed()` resolves when the last connection is gone.
    shutdown: watch::Sender<bool>,
}

impl Serving {
    fn start(
        intake: Intake<Call>,
        config: &Config,
        router: Router,
        health: Arc<StaticChecker>,
    ) -> Self {
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
            intake,
        };
        // The plugin routes come first, so a configured service cannot hide health or reflection.
        let service = ConnectRpcService::new(Chain(router, dispatcher))
            .with_deadline_policy(deadlines)
            // The default registry offers every codec that the build compiles, zstd included.
            .with_compression(CompressionRegistry::new().register(GzipProvider::default()));
        Self {
            service,
            interceptors: config.interceptors.clone(),
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
        let service = intercept(
            Service::new(MapResponse::new(self.service.clone(), status_in_trailers)),
            &self.interceptors,
        );
        let connection = serve_connection(io, info, service, self.connection.clone(), async move {
            let _ = stop.wait_for(|stop| *stop).await;
        });
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

/// Wraps `service` in `interceptors`, the first listed outermost.
fn intercept<S>(service: S, interceptors: &[impl Layer<S, Service = S>]) -> S {
    interceptors
        .iter()
        .rev()
        .fold(service, |inner, interceptor| interceptor.layer(inner))
}

/// A gRPC error carries its whole status in the trailers. connectrpc also copies the code and the message into the head, and a client that takes the status from the head, such as tonic, then drops the details and the trailers: https://github.com/connectrpc/connect-rust/issues/286. Only that copy puts these fields in a head, because PHP cannot set a `grpc-` field.
fn status_in_trailers(
    mut response: http::Response<ConnectRpcBody>,
) -> http::Response<ConnectRpcBody> {
    let headers = response.headers_mut();
    headers.remove("grpc-status");
    headers.remove("grpc-message");
    response
}

/// Runs the accept loop on the calling thread until the stop flag, then drains the connections.
pub(crate) fn serve(
    intake: Intake<Call>,
    config: Config,
    prepared: Prepared,
    worker: Worker,
) -> Result<()> {
    let stop = Stop::new().map_err(|e| anyhow!("creating the grpc stop handle: {e}"))?;
    let acceptor = Acceptor::adopt(prepared.listener, stop.handle(), &worker.handle)?;
    let mut flag = worker.stop.clone();
    worker.handle.spawn(async move {
        let _ = flag.wait_for(|stop| *stop).await;
        stop.stop();
    });
    let serving = Serving::start(intake, &config, prepared.router, prepared.health);
    let fatal = acceptor.run(&worker.handle, &serving);
    worker
        .handle
        .block_on(serving.drain(fatal, config.drain_grace))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use tower::ServiceExt;
    use tower::layer::layer_fn;
    use tower::service_fn;
    use tower::util::{BoxCloneService, BoxCloneServiceLayer};

    use super::intercept;

    type Service = BoxCloneService<http::Request<()>, http::Response<()>, Infallible>;
    type Layer = BoxCloneServiceLayer<Service, http::Request<()>, http::Response<()>, Infallible>;

    /// Appends `{name}-in` to the request and `{name}-out` to the response.
    fn tag(name: &'static str) -> Layer {
        Layer::new(layer_fn(move |inner: Service| {
            service_fn(move |mut req: http::Request<()>| {
                req.headers_mut()
                    .append("x-trace", format!("{name}-in").parse().unwrap());
                let inner = inner.clone();
                async move {
                    let mut res = inner.oneshot(req).await?;
                    res.headers_mut()
                        .append("x-trace", format!("{name}-out").parse().unwrap());
                    Ok(res)
                }
            })
        }))
    }

    /// Answers with the `x-trace` values of the request.
    fn echo() -> Service {
        Service::new(service_fn(|req: http::Request<()>| async move {
            let mut res = http::Response::new(());
            for v in req.headers().get_all("x-trace") {
                res.headers_mut().append("x-trace", v.clone());
            }
            Ok(res)
        }))
    }

    /// `Serving::spawn` wraps the connectrpc service with this fold.
    #[tokio::test(flavor = "current_thread")]
    async fn interceptors_wrap_the_service_outermost_first() {
        let service = intercept(echo(), &[tag("a"), tag("b")]);
        let res = service.oneshot(http::Request::new(())).await.unwrap();
        let trace: Vec<&str> = res
            .headers()
            .get_all("x-trace")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(trace, ["a-in", "b-in", "b-out", "a-out"]);
    }
}
