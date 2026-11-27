//! Accept loop, sharded `current_thread` `LocalSet` IO runtimes running `hyper` 1.x, and graceful drain.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use http::{Extensions, Method, Request as HttpRequest, Response, StatusCode};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use rapira_http_middleware::{MiddlewareChain, Peer, PeerAddr};
use rapira_net::{ListenAddr, Serve};
use rapira_sapi::types::Context as SapiContext;
use rapira_sapi::work::{Intake, Refused, Unit};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::LocalSet;

use crate::intake::{self, Admit, IntakeLimits, Prepared};
use crate::io::{
    ConnPhase, Inflight, InflightShard, LocalInflight, RawIo, RequestScope, ShardIo, TrackBody,
};
use crate::php::ExchangeState;
use crate::responder::{HttpResponder, ResponseBody, build_php_response, build_short_response};
use crate::{Config, Server};

/// Maximum HTTP/1.x request head size in bytes (`431 Request Header Fields Too Large` above this).
const MAX_HEADER_BYTES: usize = 64 << 10;

/// State shared across every HTTP IO shard.
pub(crate) struct Shared {
    pub cfg: Config,
    pub intake: Intake,
    pub chain: Arc<MiddlewareChain>,
    pub inflight: Arc<Inflight>,
    /// True while the listener drains. Each response then closes its connection.
    pub closing: Arc<AtomicBool>,
}

/// `!Send` state owned by one `LocalSet` IO shard to avoid cross-core atomics and `watch` mutexes.
pub(crate) struct ShardState {
    pub shared: Arc<Shared>,
    pub inflight: Rc<LocalInflight>,
    local_open: Cell<usize>,
    draining: Cell<bool>,
    drained: Rc<Notify>,
}

impl ShardState {
    #[inline]
    fn inc_open(&self) {
        self.local_open.set(self.local_open.get() + 1);
    }

    #[inline]
    fn dec_open(&self) {
        let next = self.local_open.get() - 1;
        self.local_open.set(next);
        if next == 0 && self.draining.get() {
            self.drained.notify_one();
        }
    }
}

/// `!Send` per-connection context passed by `Rc::clone` into `service_fn`, avoiding per-request `Arc` atomics.
pub(crate) struct ConnCtx {
    pub shard: Rc<ShardState>,
    pub phase: Rc<ConnPhase>,
    pub remote: PeerAddr,
    pub server: PeerAddr,
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Handles one HTTP request on the connection's IO shard.
async fn handle_request(
    conn: Rc<ConnCtx>,
    req: HttpRequest<Incoming>,
) -> Result<Response<TrackBody<ResponseBody>>, std::convert::Infallible> {
    let mut scope = RequestScope::new(Rc::clone(&conn.phase), Rc::clone(&conn.shard.inflight));
    let shared = &conn.shard.shared;
    let is_head = req.method() == Method::HEAD;

    let peer = Peer {
        remote: conn.remote.clone(),
        server: conn.server.clone(),
        https: false,
        received_at: now_unix(),
    };

    let limits = IntakeLimits {
        superglobals: shared.cfg.superglobals,
        max_body_size: shared.cfg.max_body_size,
        body_timeout: shared.cfg.keepalive_timeout,
        uploads: shared.cfg.uploads.as_ref(),
    };

    let (request, multipart, budget_bytes, extensions) =
        match intake::admit(req, peer, &shared.chain, &limits) {
            Ok(Admit::Ready(request)) => (request, None, 0, Extensions::new()),
            Ok(Admit::Pending(pending)) => match pending.finish(&shared.chain, &limits).await {
                Ok(Prepared::Short {
                    outer_depth,
                    short,
                    extensions,
                }) => {
                    let closing = shared.closing.load(Ordering::Acquire);
                    return Ok(
                        match build_short_response(
                            &shared.chain,
                            outer_depth,
                            is_head,
                            closing,
                            &extensions,
                            short,
                        ) {
                            Ok(resp) => scope.track(resp),
                            Err(status) => scope.refuse(status),
                        },
                    );
                }
                Ok(Prepared::Dispatch {
                    request,
                    multipart,
                    budget_bytes,
                    extensions,
                }) => (request, multipart, budget_bytes, extensions),
                Err(rej) => return Ok(scope.refuse(rej.status)),
            },
            Err(rej) => return Ok(scope.refuse(rej.status)),
        };

    let (responder, slot) = HttpResponder::pair(is_head);
    scope.attach_slot(slot);
    let unit = if shared.cfg.superglobals {
        Unit::Cgi(Box::new(SapiContext::new(request, Box::new(responder))))
    } else {
        Unit::Dispatcher(Box::new(ExchangeState::new(request, multipart, responder)))
    };
    if let Err(refused) = shared.intake.submit(unit, budget_bytes).await {
        let status = match refused {
            Refused::OverBudget | Refused::Saturated => StatusCode::SERVICE_UNAVAILABLE,
        };
        return Ok(scope.refuse(status));
    }
    let start = match scope.wait_start().await {
        Ok(start) => start,
        Err(()) => return Ok(scope.refuse(StatusCode::BAD_GATEWAY)),
    };

    let closing = shared.closing.load(Ordering::Acquire);
    Ok(
        match build_php_response(&shared.chain, is_head, closing, &extensions, start) {
            Ok(resp) => scope.track(resp),
            Err(status) => scope.refuse(status),
        },
    )
}

struct OpenConnGuard(Rc<ShardState>);

impl Drop for OpenConnGuard {
    fn drop(&mut self) {
        self.0.dec_open();
    }
}

enum Accepted {
    Tcp {
        stream: std::net::TcpStream,
        peer: std::net::SocketAddr,
    },
    Unix {
        stream: std::os::unix::net::UnixStream,
        peer: Option<PathBuf>,
    },
}

/// Runs one dedicated single-threaded IO shard (`current_thread` runtime + `!Send` `LocalSet`).
fn run_shard(
    shared: Arc<Shared>,
    inflight_atomic: Arc<InflightShard>,
    mut rx: mpsc::UnboundedReceiver<Accepted>,
    mut cut: watch::Receiver<bool>,
    rx_drained: oneshot::Sender<usize>,
    done: oneshot::Sender<()>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(target: "http", "failed to start HTTP IO shard runtime: {e}");
            let _ = done.send(());
            return;
        }
    };

    let mut builder = http1::Builder::new();
    builder
        .keep_alive(true)
        .auto_date_header(true)
        .writev(true)
        .max_buf_size(MAX_HEADER_BYTES)
        .max_header_size(MAX_HEADER_BYTES)
        .header_read_timeout(None);
    let builder = Rc::new(builder);

    // Cache the local address when bound to a specific non-ephemeral TCP socket to skip per-accept `getsockname`.
    let fixed_tcp_local = match shared.cfg.listen {
        ListenAddr::Tcp(a) if !a.ip().is_unspecified() && a.port() != 0 => Some(PeerAddr::Inet(a)),
        _ => None,
    };
    let unix_local = listen_addr(&shared.cfg.listen);

    let shard_state = Rc::new(ShardState {
        shared,
        inflight: Rc::new(LocalInflight::new(inflight_atomic)),
        local_open: Cell::new(0),
        draining: Cell::new(false),
        drained: Rc::new(Notify::new()),
    });

    let local = LocalSet::new();
    rt.block_on(local.run_until(async move {
        while let Some(accepted) = rx.recv().await {
            match accepted {
                Accepted::Tcp { stream, peer } => {
                    let server = match &fixed_tcp_local {
                        Some(addr) => addr.clone(),
                        None => {
                            // On macOS, getsockname fails with EINVAL on a socket that is shut down. https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/getsockname.2.html
                            match stream.local_addr() {
                                Ok(server) => PeerAddr::Inet(server),
                                Err(e) => {
                                    tracing::debug!(target: "http", "connection from {peer} dropped: {e}");
                                    continue;
                                }
                            }
                        }
                    };
                    let raw = match RawIo::from_tcp(stream) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::debug!(target: "http", "registering tcp connection from {peer}: {e}");
                            continue;
                        }
                    };
                    spawn_shard_conn(&builder, &shard_state, raw, PeerAddr::Inet(peer), server);
                }
                Accepted::Unix { stream, peer } => {
                    let raw = match RawIo::from_unix(stream) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::debug!(target: "http", "registering unix connection: {e}");
                            continue;
                        }
                    };
                    spawn_shard_conn(
                        &builder,
                        &shard_state,
                        raw,
                        PeerAddr::Unix(peer),
                        unix_local.clone(),
                    );
                }
            }
        }
        shard_state.draining.set(true);
        let _ = rx_drained.send(shard_state.local_open.get());
        if shard_state.local_open.get() > 0 {
            let drained = shard_state.drained.notified();
            tokio::select! {
                biased;
                _ = cut.wait_for(|&c| c) => {}
                () = drained => {}
            }
        }
    }));
    drop(rt);
    let _ = done.send(());
}

fn spawn_shard_conn(
    builder: &Rc<http1::Builder>,
    shard_state: &Rc<ShardState>,
    raw: RawIo,
    remote: PeerAddr,
    server: PeerAddr,
) {
    shard_state.inc_open();
    let open_guard = OpenConnGuard(Rc::clone(shard_state));
    let builder = Rc::clone(builder);
    let phase = ConnPhase::new();
    let keepalive = shard_state.shared.cfg.keepalive_timeout;
    let write_timeout = shard_state.shared.cfg.write_timeout;
    let conn = Rc::new(ConnCtx {
        shard: Rc::clone(shard_state),
        phase: Rc::clone(&phase),
        remote,
        server,
    });
    tokio::task::spawn_local(async move {
        let _open_guard = open_guard;
        let io = ShardIo::new(raw, phase, keepalive, write_timeout);
        let svc = service_fn(move |req| handle_request(Rc::clone(&conn), req));
        if let Err(err) = builder.serve_connection(io, svc).await {
            tracing::debug!(target: "http", "connection closed with error: {err}");
        }
    });
}

/// Everything the accept loop hands to the IO shards, and the drain that follows it.
struct Serving {
    shared: Arc<Shared>,
    senders: Vec<mpsc::UnboundedSender<Accepted>>,
    next: AtomicUsize,
    cut: watch::Sender<bool>,
    rx_drained_rxs: Vec<oneshot::Receiver<usize>>,
    done_rxs: Vec<oneshot::Receiver<()>>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.senders.clear();
        self.cut.send_replace(true);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Serving {
    fn start(intake: Intake, mut config: Config) -> Result<Self> {
        let cpus = thread::available_parallelism().map_or(1, usize::from);
        let num_shards = cpus.clamp(1, 16);
        let chain = Arc::new(MiddlewareChain::new(std::mem::take(&mut config.middleware)));
        let shared = Arc::new(Shared {
            cfg: config,
            intake,
            chain,
            inflight: Arc::new(Inflight::with_shards(num_shards)),
            closing: Arc::new(AtomicBool::new(false)),
        });
        let (cut, _) = watch::channel(false);
        let mut serving = Self {
            shared,
            senders: Vec::with_capacity(num_shards),
            next: AtomicUsize::new(0),
            cut,
            rx_drained_rxs: Vec::with_capacity(num_shards),
            done_rxs: Vec::with_capacity(num_shards),
            handles: Vec::with_capacity(num_shards),
        };
        for i in 0..num_shards {
            let (tx, rx) = mpsc::unbounded_channel();
            let (rx_drained_tx, rx_drained_rx) = oneshot::channel();
            let (done_tx, done_rx) = oneshot::channel();
            let shard = serving.shared.inflight.shard_at(i);
            let shared_clone = Arc::clone(&serving.shared);
            let cut_rx = serving.cut.subscribe();
            let handle = thread::Builder::new()
                .name(format!("rapira-io-{i}"))
                .spawn(move || {
                    run_shard(shared_clone, shard, rx, cut_rx, rx_drained_tx, done_tx);
                })
                .with_context(|| format!("spawning HTTP IO shard {i}"))?;
            serving.senders.push(tx);
            serving.rx_drained_rxs.push(rx_drained_rx);
            serving.done_rxs.push(done_rx);
            serving.handles.push(handle);
        }
        Ok(serving)
    }

    /// Assigns accepted connections round-robin across IO shards without cross-core counter scans.
    fn dispatch(&self, conn: Accepted) {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        let _ = self.senders[idx].send(conn);
    }

    async fn join_shards(&mut self) {
        self.senders.clear();
        self.cut.send_replace(true);
        let handles = std::mem::take(&mut self.handles);
        let _ = tokio::task::spawn_blocking(move || {
            for handle in handles {
                let _ = handle.join();
            }
        })
        .await;
    }

    /// A held connection closes after its next response. An idle connection can stay open for the full grace period.
    async fn drain(mut self, grace: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + grace;
        self.shared.closing.store(true, Ordering::Release);
        self.senders.clear();
        let rx_drained_rxs = std::mem::take(&mut self.rx_drained_rxs);
        let mut open = 0usize;
        let _ = tokio::time::timeout_at(deadline, async {
            for rx in rx_drained_rxs {
                if let Ok(n) = rx.await {
                    open += n;
                }
            }
        })
        .await;
        tracing::info!(target: "http", "draining {open} open connections");
        let done_rxs = std::mem::take(&mut self.done_rxs);
        let _ = tokio::time::timeout_at(deadline, async move {
            for rx in done_rxs {
                let _ = rx.await;
            }
        })
        .await;
        let stranded = self.shared.inflight.count();
        self.join_shards().await;
        if stranded > 0 {
            return Err(anyhow!(
                "http drain timed out after {grace:?} with {stranded} request(s) in flight; \
                 their responses were cut short"
            ));
        }
        tracing::info!(target: "http", "drained cleanly; accept loop stopped");
        Ok(())
    }

    async fn abort(mut self) {
        self.join_shards().await;
    }
}

impl Serve for Serving {
    fn spawn_tcp(&self, stream: std::net::TcpStream, peer: std::net::SocketAddr) {
        self.dispatch(Accepted::Tcp { stream, peer });
    }

    fn spawn_unix(&self, stream: std::os::unix::net::UnixStream, peer: Option<PathBuf>) {
        self.dispatch(Accepted::Unix { stream, peer });
    }
}

pub(crate) async fn serve(
    server: Server,
    intake: Intake,
    stop: watch::Receiver<bool>,
    drain_grace: Duration,
) -> Result<()> {
    let Server {
        config,
        prepared,
        spool: _spool,
    } = server;
    let serving = Serving::start(intake, config)?;
    match prepared.run(stop, &serving).await {
        Ok(()) => serving.drain(drain_grace).await,
        Err(e) => {
            serving.abort().await;
            Err(e)
        }
    }
}

fn listen_addr(listen: &ListenAddr) -> PeerAddr {
    match listen {
        ListenAddr::Tcp(a) => PeerAddr::Inet(*a),
        ListenAddr::Unix(p) => PeerAddr::Unix(Some(p.clone())),
    }
}
