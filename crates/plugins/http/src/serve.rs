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
use tokio::sync::watch::{self, channel};

use crate::Config;
#[cfg(target_os = "linux")]
use crate::accept_linux::{TcpListener, UnixListener};
use crate::handler::{RapiraService, Shared};

enum Acceptor {
    Tcp(TcpListener),
    Unix(UnixListener),
}

fn create_acceptor(prepared: PreparedListener) -> Result<Acceptor> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    let tcp: bool = matches!(prepared.addr(), ListenAddr::Tcp(_));
    // SAFETY: into_raw_fd transfers sole ownership of a listening socket; prepare
    // already set O_NONBLOCK, which from_std requires but does not set.
    if tcp {
        let std = unsafe { std::net::TcpListener::from_raw_fd(prepared.into_raw_fd()) };
        Ok(Acceptor::Tcp(TcpListener::from_std(std)?))
    } else {
        let std = unsafe { std::os::unix::net::UnixListener::from_raw_fd(prepared.into_raw_fd()) };
        Ok(Acceptor::Unix(UnixListener::from_std(std)?))
    }
}

pub(crate) async fn serve(
    php: Php,
    config: Config,
    prepared: PreparedListener,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let acceptor = create_acceptor(prepared)?;
    match &config.listen {
        ListenAddr::Tcp(a) => tracing::info!(target: "http", "listening on http://{a}"),
        ListenAddr::Unix(p) => tracing::info!(target: "http", "listening on unix:{}", p.display()),
    }

    let chain: Arc<[_]> = config.middleware.clone().into();
    let inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let shared = Arc::new(Shared {
        cfg: config,
        php,
        chain,
        inflight: Arc::clone(&inflight),
    });
    let graceful = GracefulShutdown::new();

    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(shared.cfg.keepalive_timeout)
        .preserve_header_case(false)
        .half_close(false)
        .keep_alive(true);
    let mut fatal: Option<anyhow::Error> = None;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => break,
            res = accept_connection(&acceptor, &shared.cfg.listen, &builder, &graceful, &shared) => match res {
                Ok(()) => {
                    #[cfg(target_os = "linux")]
                    if let Err(e) = match &acceptor {
                        Acceptor::Tcp(listener) => listener.on_accept(),
                        Acceptor::Unix(listener) => listener.on_accept(),
                    } {
                        fatal = Some(anyhow!("listener rotation failed: {e}"));
                        break;
                    }
                }
                Err(e) if is_fatal_accept(&e) => {
                    fatal = Some(anyhow!("listener failed: {e}"));
                    break;
                }
                Err(e) if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::Interrupted
                ) => {
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
    let deadline = tokio::time::Instant::now() + shared.cfg.drain_grace;
    if tokio::time::timeout_at(deadline, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            target: "http",
            "graceful connection shutdown did not finish within {:?}",
            shared.cfg.drain_grace
        );
    }
    while inflight.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stranded = inflight.load(Ordering::Acquire);
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
            shared.cfg.drain_grace
        ));
    }
    tracing::info!(target: "http", "drained cleanly; accept loop stopped");
    Ok(())
}

fn listen_addr(listen: &ListenAddr) -> Addr {
    match listen {
        ListenAddr::Tcp(a) => Addr::Inet(*a),
        ListenAddr::Unix(p) => Addr::Unix(Some(p.clone())),
    }
}

// Linux accept() forwards pending network errors of the new connection, so only errnos
// that prove listener state are fatal. https://man7.org/linux/man-pages/man2/accept.2.html
fn is_fatal_accept(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK)
    )
}

async fn accept_connection(
    acceptor: &Acceptor,
    listen: &ListenAddr,
    builder: &http1::Builder,
    graceful: &GracefulShutdown,
    shared: &Arc<Shared>,
) -> std::io::Result<()> {
    match acceptor {
        Acceptor::Tcp(l) => {
            let (stream, peer) = l.accept().await?;
            let _ = stream.set_nodelay(true);
            let server = stream
                .local_addr()
                .map(Addr::Inet)
                .unwrap_or_else(|_| listen_addr(listen));
            spawn_conn(stream, Addr::Inet(peer), server, builder, graceful, shared);
        }
        Acceptor::Unix(l) => {
            let (stream, peer) = l.accept().await?;
            let remote = Addr::Unix(peer.as_pathname().map(Into::into));
            let server = listen_addr(listen);
            spawn_conn(stream, remote, server, builder, graceful, shared);
        }
    }
    Ok(())
}

fn spawn_conn<S>(
    stream: S,
    remote: Addr,
    server: Addr,
    builder: &http1::Builder,
    graceful: &GracefulShutdown,
    shared: &Arc<Shared>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (closed_tx, closed_rx) = channel(crate::bridge::ConnectionState::default());
    let svc = RapiraService::new(Arc::clone(shared), remote, server, closed_rx);
    let io = crate::bridge::TimedIo::new(
        TokioIo::new(stream),
        shared.cfg.write_timeout,
        closed_tx.clone(),
    );
    let connection = builder.serve_connection(io, svc);
    let watched = graceful.watch(connection);
    tokio::spawn(async move {
        if let Err(e) = watched.await {
            tracing::debug!(target: "http", "connection ended with error: {e}");
        }
        closed_tx.send_modify(|s| s.closed = true);
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::future::{Future, poll_fn};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::task::Poll;

    use tokio::io::AsyncReadExt;

    use super::*;

    fn assert_nonblocking(stream: &impl AsRawFd) {
        // SAFETY: fcntl reads flags from the live stream descriptor.
        let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
    }

    #[tokio::test]
    async fn queued_tcp_connections_remain_ready_after_each_accept() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut clients = Vec::new();
        for byte in 0..16_u8 {
            let mut client = std::net::TcpStream::connect(addr).unwrap();
            client.write_all(&[byte]).unwrap();
            clients.push(client);
        }
        let listener = TcpListener::from_std(listener).unwrap();
        let mut received = BTreeSet::new();
        for _ in 0..16 {
            let (mut stream, peer) =
                tokio::time::timeout(Duration::from_secs(2), listener.accept())
                    .await
                    .expect("queued connection must remain ready")
                    .unwrap();
            assert!(
                clients
                    .iter()
                    .any(|client| client.local_addr().unwrap() == peer)
            );
            assert_nonblocking(&stream);
            received.insert(stream.read_u8().await.unwrap());
            #[cfg(target_os = "linux")]
            listener.on_accept().unwrap();
        }
        assert_eq!(received, (0..16).collect());
    }

    #[tokio::test]
    async fn canceled_tcp_accept_keeps_the_next_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = TcpListener::from_std(listener).unwrap();
        let mut pending = Box::pin(listener.accept());
        assert!(poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_pending())).await);
        drop(pending);
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (_, peer) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("accept must remain usable after cancellation")
            .unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
    }

    #[tokio::test]
    async fn queued_unix_connections_remain_ready_after_each_accept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut clients = Vec::new();
        for byte in 0..16_u8 {
            let mut client = std::os::unix::net::UnixStream::connect(&path).unwrap();
            client.write_all(&[byte]).unwrap();
            clients.push(client);
        }
        let listener = UnixListener::from_std(listener).unwrap();
        let mut received = BTreeSet::new();
        for _ in 0..16 {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .expect("queued connection must remain ready")
                .unwrap();
            assert_nonblocking(&stream);
            received.insert(stream.read_u8().await.unwrap());
            #[cfg(target_os = "linux")]
            listener.on_accept().unwrap();
        }
        assert_eq!(received, (0..16).collect());
    }
}
