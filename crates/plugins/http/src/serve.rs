use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use extension_api::{Addr, ListenAddr, Php, PreparedListener, Result};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(not(target_os = "linux"))]
use tokio::net::{TcpListener, UnixListener};
#[cfg(not(target_os = "linux"))]
use tokio::sync::watch;
use tokio::sync::watch::channel;

use crate::Config;
#[cfg(target_os = "linux")]
use crate::accept_linux::{TcpListener, UnixListener};
use crate::handler::{RapiraService, Shared};

/// Stops the accept loop. The blocked acceptor waits on this eventfd.
#[cfg(target_os = "linux")]
pub(crate) type Stop = crate::accept_linux::Wake;

/// Stops the accept loop. The async acceptor selects on this flag.
#[cfg(not(target_os = "linux"))]
pub(crate) struct Stop(watch::Sender<bool>);

#[cfg(not(target_os = "linux"))]
impl Stop {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self(watch::channel(false).0))
    }

    pub(crate) fn handle(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }

    pub(crate) fn stop(&self) {
        let _ = self.0.send(true);
    }
}

enum Acceptor {
    Tcp(TcpListener),
    Unix(UnixListener),
}

fn create_acceptor(
    prepared: PreparedListener,
    #[cfg(target_os = "linux")] stop: Stop,
) -> Result<Acceptor> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    let tcp: bool = matches!(prepared.addr(), ListenAddr::Tcp(_));
    // SAFETY: into_raw_fd transfers sole ownership of a listening socket.
    // prepare set O_NONBLOCK: both acceptors need an accept that does not block.
    if tcp {
        let std = unsafe { std::net::TcpListener::from_raw_fd(prepared.into_raw_fd()) };
        #[cfg(target_os = "linux")]
        let listener = TcpListener::from_std(std, stop)?;
        #[cfg(not(target_os = "linux"))]
        let listener = TcpListener::from_std(std)?;
        Ok(Acceptor::Tcp(listener))
    } else {
        let std = unsafe { std::os::unix::net::UnixListener::from_raw_fd(prepared.into_raw_fd()) };
        #[cfg(target_os = "linux")]
        let listener = UnixListener::from_std(std, stop)?;
        #[cfg(not(target_os = "linux"))]
        let listener = UnixListener::from_std(std)?;
        Ok(Acceptor::Unix(listener))
    }
}

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    shared: Arc<Shared>,
    graceful: GracefulShutdown,
    builder: http1::Builder,
}

impl Serving {
    fn start(php: Php, config: Config) -> Self {
        match &config.listen {
            ListenAddr::Tcp(a) => tracing::info!(target: "http", "listening on http://{a}"),
            ListenAddr::Unix(p) => {
                tracing::info!(target: "http", "listening on unix:{}", p.display())
            }
        }
        let chain: Arc<[_]> = config.middleware.clone().into();
        let shared = Arc::new(Shared {
            cfg: config,
            php,
            chain,
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

    fn spawn_tcp(&self, stream: tokio::net::TcpStream, peer: std::net::SocketAddr) {
        let _ = stream.set_nodelay(true);
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

    fn spawn_conn<S>(&self, stream: S, remote: Addr, server: Addr)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (closed_tx, closed_rx) = channel(crate::bridge::ConnectionState::default());
        let svc = RapiraService::new(Arc::clone(&self.shared), remote, server, closed_rx);
        let io = crate::bridge::TimedIo::new(
            TokioIo::new(stream),
            self.shared.cfg.write_timeout,
            closed_tx.clone(),
        );
        let connection = self.builder.serve_connection(io, svc);
        let watched = self.graceful.watch(connection);
        tokio::spawn(async move {
            if let Err(e) = watched.await {
                tracing::debug!(target: "http", "connection ended with error: {e}");
            }
            closed_tx.send_modify(|s| s.closed = true);
        });
    }

    /// Waits out the connections in flight. The acceptor is already gone.
    async fn drain(self, fatal: Option<anyhow::Error>) -> Result<()> {
        let deadline = tokio::time::Instant::now() + self.shared.cfg.drain_grace;
        if tokio::time::timeout_at(deadline, self.graceful.shutdown())
            .await
            .is_err()
        {
            tracing::warn!(
                target: "http",
                "graceful connection shutdown did not finish within {:?}",
                self.shared.cfg.drain_grace
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
                "http drain timed out after {:?} with {stranded} request(s) in flight; \
                 their responses were cut short",
                self.shared.cfg.drain_grace
            ));
        }
        tracing::info!(target: "http", "drained cleanly; accept loop stopped");
        Ok(())
    }
}

/// Runs the accept loop on the calling thread. A blocked accept is what lets the kernel
/// hand each connection to one worker.
#[cfg(target_os = "linux")]
pub(crate) fn serve(
    php: Php,
    config: Config,
    prepared: PreparedListener,
    stop: Stop,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let acceptor = create_acceptor(prepared, stop)?;
    let serving = Serving::start(php, config);
    let mut fatal: Option<anyhow::Error> = None;
    {
        // tokio::spawn and from_std reach the runtime the connections run on.
        let _guard = rt.enter();
        loop {
            match accept_blocking(&acceptor, &serving) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) if is_fatal_accept(&e) => {
                    fatal = Some(anyhow!("listener failed: {e}"));
                    break;
                }
                Err(e) if is_skipped_accept(&e) => {
                    tracing::debug!(target: "http", "accept skipped: {e}");
                }
                Err(e) => {
                    tracing::warn!(target: "http", "accept failed: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    drop(acceptor);
    rt.block_on(serving.drain(fatal))
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn serve(
    php: Php,
    config: Config,
    prepared: PreparedListener,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let acceptor = create_acceptor(prepared)?;
    let serving = Serving::start(php, config);
    let mut fatal: Option<anyhow::Error> = None;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => break,
            res = accept_connection(&acceptor, &serving) => match res {
                Ok(()) => {}
                Err(e) if is_fatal_accept(&e) => {
                    fatal = Some(anyhow!("listener failed: {e}"));
                    break;
                }
                Err(e) if is_skipped_accept(&e) => {
                    tracing::debug!(target: "http", "accept skipped: {e}");
                }
                Err(e) => {
                    tracing::warn!(target: "http", "accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    drop(acceptor);
    serving.drain(fatal).await
}

fn listen_addr(listen: &ListenAddr) -> Addr {
    match listen {
        ListenAddr::Tcp(a) => Addr::Inet(*a),
        ListenAddr::Unix(p) => Addr::Unix(Some(p.clone())),
    }
}

// Linux accept() forwards pending network errors of the new connection, so only errnos
// that prove listener state are fatal. https://man7.org/linux/man-pages/man2/accept.2.html
// `ErrorKind::Other` never carries an errno; it is the wrapped rotation failure, after
// which the listener is unregistered.
fn is_fatal_accept(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::Other
        || matches!(
            e.raw_os_error(),
            Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK)
        )
}

fn is_skipped_accept(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
    )
}

/// Takes one connection. False means the wake descriptor stopped the loop.
#[cfg(target_os = "linux")]
fn accept_blocking(acceptor: &Acceptor, serving: &Serving) -> std::io::Result<bool> {
    match acceptor {
        Acceptor::Tcp(l) => match l.accept_blocking()? {
            None => return Ok(false),
            Some((stream, peer)) => {
                serving.spawn_tcp(tokio::net::TcpStream::from_std(stream)?, peer);
            }
        },
        Acceptor::Unix(l) => match l.accept_blocking()? {
            None => return Ok(false),
            Some((stream, peer)) => {
                serving.spawn_unix(
                    tokio::net::UnixStream::from_std(stream)?,
                    peer.as_pathname(),
                );
            }
        },
    }
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
async fn accept_connection(acceptor: &Acceptor, serving: &Serving) -> std::io::Result<()> {
    match acceptor {
        Acceptor::Tcp(l) => {
            let (stream, peer) = l.accept().await?;
            serving.spawn_tcp(stream, peer);
        }
        Acceptor::Unix(l) => {
            let (stream, peer) = l.accept().await?;
            serving.spawn_unix(stream, peer.as_pathname());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        error: std::io::Error,
        fatal: bool,
        skipped: bool,
    }

    /// Only an error that proves the listener is unusable ends the accept loop.
    #[test]
    fn accept_errors_end_the_loop_only_when_the_listener_is_gone() {
        let cases = [
            Case {
                name: "a rotation failure leaves the listener unregistered",
                error: std::io::Error::other("listener rotation failed"),
                fatal: true,
                skipped: false,
            },
            Case {
                name: "EBADF proves the listener descriptor is gone",
                error: std::io::Error::from_raw_os_error(libc::EBADF),
                fatal: true,
                skipped: false,
            },
            Case {
                name: "EMFILE is a limit of this worker, not of the listener",
                error: std::io::Error::from_raw_os_error(libc::EMFILE),
                fatal: false,
                skipped: false,
            },
            Case {
                name: "ECONNABORTED concerns one connection",
                error: std::io::Error::from_raw_os_error(libc::ECONNABORTED),
                fatal: false,
                skipped: true,
            },
        ];
        for case in cases {
            assert_eq!(is_fatal_accept(&case.error), case.fatal, "{}", case.name);
            assert_eq!(
                is_skipped_accept(&case.error),
                case.skipped,
                "{}",
                case.name
            );
        }
    }
}
