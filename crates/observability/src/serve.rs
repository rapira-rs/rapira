use std::convert::Infallible;
use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use rapira_net::{Acceptor, PrepareCtx, PreparedListener, Serve};
use rapira_scoreboard::{PoolRegion, Scoreboard};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::config::Settings;
use crate::text::{self, Build};
use crate::{memory, probes, stats};

/// The observability endpoint. The master prepares it before the fork, and the observability process serves it.
pub struct Server {
    build: Build,
    prepared: PreparedListener,
    keepalive_timeout: Duration,
    metrics: bool,
    probes: bool,
}

impl Server {
    /// Master side, before the fork: binds the listener.
    pub fn new(settings: Settings, build: Build, ctx: &mut PrepareCtx) -> Result<Server> {
        let prepared = ctx.bind(&settings.listen)?;
        tracing::info!(target: "observability", "prepared listener on {}", prepared.addr());
        Ok(Self {
            build,
            prepared,
            keepalive_timeout: settings.keepalive_timeout,
            metrics: settings.metrics,
            probes: settings.probes,
        })
    }

    /// In the observability process. Serves the routes of the configured sub-tables until `stop` turns true, then waits for the requests in flight within `drain_grace`. `pools` are the pools that the metrics and `/readyz` report.
    pub fn serve(
        self,
        board: Scoreboard,
        pools: &'static [PoolRegion],
        stop: watch::Receiver<bool>,
        drain_grace: Duration,
    ) -> Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("rapira-obs-io")
            .build()
            .map_err(|e| anyhow!("building the observability runtime: {e}"))?;
        tracing::info!(target: "observability", "serving on {}", self.prepared.addr());
        let acceptor = Acceptor::adopt(self.prepared, stop, rt.handle())?;
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(self.keepalive_timeout);
        let serving = Serving {
            routes: Box::leak(Box::new(Routes {
                board,
                pools,
                build: self.build,
                metrics: self.metrics,
                probes: self.probes,
            })),
            graceful: GracefulShutdown::new(),
            builder,
        };
        let fatal = acceptor.run(rt.handle(), &serving);
        let Serving { graceful, .. } = serving;
        // `tokio::time::timeout` panics outside a runtime context, so the call runs inside `block_on`.
        let drained =
            rt.block_on(async { tokio::time::timeout(drain_grace, graceful.shutdown()).await });
        if drained.is_err() {
            tracing::warn!(target: "observability", "requests still in flight after {drain_grace:?}");
        }
        match fatal {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// What a request reads: the board, the pools it reports, the build and the routes that the config turns on.
struct Routes {
    board: Scoreboard,
    /// Every pool except the pool of the observability process.
    pools: &'static [PoolRegion],
    build: Build,
    metrics: bool,
    probes: bool,
}

impl Routes {
    /// One pass over the board, then one `/proc` read for each live worker.
    fn metrics_text(&self) -> String {
        let mut pools = stats::board_stats(&self.board, self.pools);
        for worker in pools.iter_mut().flat_map(|p| p.workers.iter_mut()) {
            worker.memory = memory::read(worker.pid);
        }
        text::render(&pools, &self.build)
    }
}

struct Serving {
    routes: &'static Routes,
    graceful: GracefulShutdown,
    builder: http1::Builder,
}

impl Serving {
    fn spawn_conn<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let routes = self.routes;
        let service = hyper::service::service_fn(move |req| {
            std::future::ready(Ok::<_, Infallible>(respond(routes, &req)))
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
fn respond(routes: &Routes, req: &Request<Incoming>) -> Response<Full<Bytes>> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") if routes.metrics => {
            reply(StatusCode::OK, text::CONTENT_TYPE, routes.metrics_text())
        }
        // The lifeline stops this process when the master dies, so an answer shows that the master lives.
        (&Method::GET, "/livez") if routes.probes => {
            reply(StatusCode::OK, PROBE_CONTENT_TYPE, "ok\n")
        }
        (&Method::GET, "/readyz") if routes.probes => {
            let unready = probes::unready(&routes.board, routes.pools);
            if unready.is_empty() {
                reply(StatusCode::OK, PROBE_CONTENT_TYPE, "ok\n")
            } else {
                let body: String = unready
                    .iter()
                    .map(|name| format!("pool {name}: no ready worker\n"))
                    .collect();
                reply(StatusCode::SERVICE_UNAVAILABLE, PROBE_CONTENT_TYPE, body)
            }
        }
        _ => {
            let mut not_found = Response::new(Full::new(Bytes::new()));
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            not_found
        }
    }
}

fn reply(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(body.into()));
    *res.status_mut() = status;
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}
