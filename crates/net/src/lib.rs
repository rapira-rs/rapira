use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::watch;

pub mod listen;
pub use listen::{ListenAddr, PreparedListener, bind};

/// The accepted connections of one listener. Dropping the owner cuts open connections.
#[derive(Default)]
pub struct Connections(watch::Sender<()>);

impl Connections {
    pub fn spawn(&self, connection: impl Future<Output = ()> + Send + 'static) {
        let mut cut = self.0.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = cut.changed() => {}
                () = connection => {}
            }
        });
    }

    pub fn count(&self) -> usize {
        self.0.receiver_count()
    }

    /// Waits for the sockets and any work that outlives them under one deadline.
    pub async fn drain(&self, grace: Duration, finish: impl Future<Output = ()>) -> bool {
        tokio::time::timeout(grace, async {
            self.0.closed().await;
            finish.await;
        })
        .await
        .is_ok()
    }
}

/// Starts a task for each connection that [`PreparedListener::run`] accepts.
pub trait Serve {
    fn spawn_tcp(&self, stream: tokio::net::TcpStream, peer: std::net::SocketAddr);
    fn spawn_unix(&self, stream: tokio::net::UnixStream, peer: Option<&std::path::Path>);
}

enum Socket {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl PreparedListener {
    /// Accepts connections until `stop` is true or its sender closes. The listener closes before this returns.
    pub async fn run(self, mut stop: watch::Receiver<bool>, serve: &impl Serve) -> Result<()> {
        let Self { fd, addr } = self;
        let accept = async {
            let socket = match addr {
                ListenAddr::Tcp(_) => TcpListener::from_std(fd.into()).map(Socket::Tcp),
                ListenAddr::Unix(_) => UnixListener::from_std(fd.into()).map(Socket::Unix),
            }
            .with_context(|| format!("registering listener on {addr}"))?;
            loop {
                match accept_connection(&socket, serve).await {
                    Ok(()) => {}
                    Err(e) if is_fatal_accept(&e) => {
                        return Err(e).with_context(|| format!("listener failed on {addr}"));
                    }
                    Err(e) if is_skipped_accept(&e) => {
                        tracing::debug!(target: "net", "accept skipped on {addr}: {e}");
                    }
                    Err(e) => {
                        tracing::warn!(target: "net", "accept failed on {addr}: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        };
        tokio::select! {
            biased;
            _ = stop.wait_for(|stop| *stop) => Ok(()),
            result = accept => result,
        }
    }
}

// Linux accept() also reports network errors from individual connections. https://man7.org/linux/man-pages/man2/accept.2.html
fn is_fatal_accept(e: &std::io::Error) -> bool {
    matches!(
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

async fn accept_connection(socket: &Socket, serve: &impl Serve) -> std::io::Result<()> {
    match socket {
        Socket::Tcp(l) => {
            let (stream, peer) = l.accept().await?;
            let _ = stream.set_nodelay(true);
            serve.spawn_tcp(stream, peer);
        }
        Socket::Unix(l) => {
            let (stream, peer) = l.accept().await?;
            serve.spawn_unix(stream, peer.as_pathname());
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
                name: "EBADF proves the listener descriptor is gone",
                error: std::io::Error::from_raw_os_error(libc::EBADF),
                fatal: true,
                skipped: false,
            },
            Case {
                name: "EINVAL means the socket is not listening",
                error: std::io::Error::from_raw_os_error(libc::EINVAL),
                fatal: true,
                skipped: false,
            },
            Case {
                name: "ENOTSOCK means the descriptor is not a socket",
                error: std::io::Error::from_raw_os_error(libc::ENOTSOCK),
                fatal: true,
                skipped: false,
            },
            Case {
                name: "EMFILE is the file descriptor limit of the process",
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
