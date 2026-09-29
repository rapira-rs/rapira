use std::convert::Infallible;
use std::sync::Arc;
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
use crate::{memory, stats};

/// The metrics endpoint. The master prepares it before the fork, and the metrics process serves it.
pub struct Server {
    build: Build,
    prepared: PreparedListener,
    keepalive_timeout: Duration,
}

impl Server {
    /// Master side, before the fork: binds the listener.
    pub fn new(settings: Settings, build: Build, ctx: &mut PrepareCtx) -> Result<Server> {
        let prepared = ctx.bind(&settings.listen)?;
        tracing::info!(target: "metrics", "prepared listener on {}", prepared.addr());
        Ok(Self {
            build,
            prepared,
            keepalive_timeout: settings.keepalive_timeout,
        })
    }

    /// In the metrics process. Serves `GET /metrics` until `stop` turns true, then waits for the scrapes in flight within `drain_grace`. `own` is the pool of the metrics process, which the output leaves out.
    pub fn serve(
        self,
        board: Scoreboard,
        regions: &'static [PoolRegion],
        own: usize,
        stop: watch::Receiver<bool>,
        drain_grace: Duration,
    ) -> Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("rapira-metrics-io")
            .build()
            .map_err(|e| anyhow!("building the metrics runtime: {e}"))?;
        tracing::info!(target: "metrics", "serving /metrics on {}", self.prepared.addr());
        let acceptor = Acceptor::adopt(self.prepared, stop, rt.handle())?;
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(self.keepalive_timeout);
        let serving = Serving {
            scrape: Arc::new(Scrape {
                board,
                regions,
                own,
                build: self.build,
            }),
            graceful: GracefulShutdown::new(),
            builder,
        };
        let fatal = acceptor.run(rt.handle(), &serving);
        let Serving { graceful, .. } = serving;
        // `tokio::time::timeout` panics outside a runtime context, so the call runs inside `block_on`.
        let drained =
            rt.block_on(async { tokio::time::timeout(drain_grace, graceful.shutdown()).await });
        if drained.is_err() {
            tracing::warn!(target: "metrics", "scrapes still in flight after {drain_grace:?}");
        }
        match fatal {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// What one scrape reads.
struct Scrape {
    board: Scoreboard,
    regions: &'static [PoolRegion],
    own: usize,
    build: Build,
}

impl Scrape {
    /// One pass over the board, then one `/proc` read for each live worker.
    fn text(&self) -> String {
        let mut pools = stats::board_stats(&self.board, self.regions, self.own);
        for worker in pools.iter_mut().flat_map(|p| p.workers.iter_mut()) {
            worker.memory = memory::read(worker.pid);
        }
        text::render(&pools, &self.build)
    }
}

struct Serving {
    scrape: Arc<Scrape>,
    graceful: GracefulShutdown,
    builder: http1::Builder,
}

impl Serving {
    fn spawn_conn<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let scrape = Arc::clone(&self.scrape);
        let service = hyper::service::service_fn(move |req| {
            let scrape = Arc::clone(&scrape);
            async move { Ok::<_, Infallible>(respond(&scrape, &req)) }
        });
        let conn = self
            .graceful
            .watch(self.builder.serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(target: "metrics", "connection ended with error: {e}");
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

/// `GET /metrics` answers the text format. Every other method or path gets 404.
fn respond(scrape: &Scrape, req: &Request<Incoming>) -> Response<Full<Bytes>> {
    if req.method() != Method::GET || req.uri().path() != "/metrics" {
        let mut not_found = Response::new(Full::new(Bytes::new()));
        *not_found.status_mut() = StatusCode::NOT_FOUND;
        return not_found;
    }
    let mut ok = Response::new(Full::new(Bytes::from(scrape.text())));
    ok.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(text::CONTENT_TYPE));
    ok
}
