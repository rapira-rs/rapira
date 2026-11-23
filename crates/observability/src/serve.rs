use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use rapira_net::{PreparedListener, Serve};
use rapira_sapi::pool::Pool;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::config::Settings;
use crate::text::{self, Build};
use crate::{probes, stats};

/// The bound listener and its endpoint settings.
pub struct Server {
    prepared: PreparedListener,
    keepalive_timeout: Duration,
    metrics: bool,
    probes: bool,
}

impl Server {
    /// Binds the listener before the PHP engine starts.
    pub fn from_settings(settings: Settings) -> Result<Self> {
        let prepared = rapira_net::bind(&settings.listen)?;
        Ok(Self {
            prepared,
            keepalive_timeout: settings.keepalive_timeout,
            metrics: settings.metrics,
            probes: settings.probes,
        })
    }

    /// Runs on the observability thread. When `stop` is true, waits up to `drain_grace` for open connections to close.
    pub fn serve(
        self,
        pools: Arc<[Arc<Pool>]>,
        build: Build,
        stop: watch::Receiver<bool>,
        drain_grace: Duration,
    ) -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building the observability runtime")?;
        rt.block_on(async {
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(self.keepalive_timeout);
            let serving = Serving {
                routes: Arc::new(Routes {
                    pools,
                    build,
                    metrics: self.metrics,
                    probes: self.probes,
                }),
                graceful: GracefulShutdown::new(),
                builder,
            };
            tracing::info!(target: "observability", "listening on {}", self.prepared.addr());
            let result = self.prepared.run(stop, &serving).await;
            if tokio::time::timeout(drain_grace, serving.graceful.shutdown()).await.is_err() {
                tracing::warn!(target: "observability", "requests still in flight after {drain_grace:?}");
            }
            result
        })
    }
}

struct Routes {
    pools: Arc<[Arc<Pool>]>,
    build: Build,
    metrics: bool,
    probes: bool,
}

struct Serving {
    routes: Arc<Routes>,
    graceful: GracefulShutdown,
    builder: http1::Builder,
}

impl Serving {
    fn spawn_conn<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let routes = Arc::clone(&self.routes);
        let service = hyper::service::service_fn(move |req| {
            std::future::ready(Ok::<_, Infallible>(respond(&routes, &req)))
        });
        let conn = self
            .graceful
            .watch(self.builder.serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(target: "observability", "connection ended with error: {e}");
            }
        });
    }
}

impl Serve for Serving {
    fn spawn_tcp(&self, stream: tokio::net::TcpStream, _peer: std::net::SocketAddr) {
        self.spawn_conn(stream);
    }

    fn spawn_unix(&self, stream: tokio::net::UnixStream, _peer: Option<&std::path::Path>) {
        self.spawn_conn(stream);
    }
}

/// The content type of the probe answers.
const PROBE_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// `GET /metrics` answers the text format when `[observability.metrics]` is configured, and `GET /livez` and `GET /readyz` answer when `[observability.probes]` is configured. Every other request gets 404.
fn respond(routes: &Routes, req: &Request<Incoming>) -> Response<String> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") if routes.metrics => {
            let pools: Vec<_> = routes
                .pools
                .iter()
                .map(|pool| stats::pool_stats(&pool.snapshot()))
                .collect();
            reply(
                StatusCode::OK,
                text::CONTENT_TYPE,
                text::render(&pools, &routes.build),
            )
        }
        (&Method::GET, "/livez") if routes.probes => {
            reply(StatusCode::OK, PROBE_CONTENT_TYPE, "ok\n")
        }
        (&Method::GET, "/readyz") if routes.probes => {
            let pools: Vec<_> = routes.pools.iter().map(|pool| pool.snapshot()).collect();
            let (status, body) = probes::readyz(&pools);
            reply(status, PROBE_CONTENT_TYPE, body)
        }
        _ => {
            let mut not_found = Response::new(String::new());
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            not_found
        }
    }
}

fn reply(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<String>,
) -> Response<String> {
    let mut res = Response::new(body.into());
    *res.status_mut() = status;
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}
