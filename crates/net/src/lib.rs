use std::time::Duration;

use anyhow::anyhow;
#[cfg(not(target_os = "linux"))]
use tokio::net::{TcpListener, UnixListener};
use tokio::runtime::Handle;
#[cfg(not(target_os = "linux"))]
use tokio::sync::watch;

#[cfg(target_os = "linux")]
mod accept_linux;

pub mod listen;
pub use listen::{ListenAddr, PrepareCtx, PreparedListener};

#[cfg(target_os = "linux")]
use accept_linux::{TcpListener, UnixListener};

/// Stops the accept loop. The blocked acceptor waits on this eventfd.
#[cfg(target_os = "linux")]
pub type Stop = accept_linux::Wake;

#[cfg(target_os = "linux")]
pub type StopHandle = accept_linux::Wake;

/// Stops the accept loop. The async acceptor selects on this flag.
#[cfg(not(target_os = "linux"))]
pub struct Stop(watch::Sender<bool>);

#[cfg(not(target_os = "linux"))]
pub type StopHandle = watch::Receiver<bool>;

#[cfg(not(target_os = "linux"))]
impl Stop {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self(watch::channel(false).0))
    }

    pub fn handle(&self) -> StopHandle {
        self.0.subscribe()
    }

    pub fn stop(&self) {
        let _ = self.0.send(true);
    }
}

/// Takes the accepted connections. [`Acceptor::run`] calls it inside its runtime.
pub trait Serve {
    fn spawn_tcp(&self, stream: tokio::net::TcpStream, peer: std::net::SocketAddr);
    fn spawn_unix(&self, stream: tokio::net::UnixStream, peer: Option<&std::path::Path>);
}

/// The listening socket of one worker.
pub struct Acceptor {
    socket: Socket,
    addr: ListenAddr,
    #[cfg(not(target_os = "linux"))]
    stop: StopHandle,
}

enum Socket {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Acceptor {
    pub fn adopt(
        prepared: PreparedListener,
        stop: StopHandle,
        rt: &Handle,
    ) -> std::io::Result<Self> {
        use std::os::fd::{FromRawFd, IntoRawFd};
        let addr = prepared.addr().clone();
        let tcp: bool = matches!(addr, ListenAddr::Tcp(_));
        // On other OSes from_std registers the tokio listener with the reactor of rt.
        let _guard = rt.enter();
        // SAFETY: into_raw_fd transfers sole ownership of a listening socket.
        // prepare set O_NONBLOCK: both acceptors need an accept that does not block.
        let socket = if tcp {
            let std = unsafe { std::net::TcpListener::from_raw_fd(prepared.into_raw_fd()) };
            #[cfg(target_os = "linux")]
            let listener = TcpListener::from_std(std, stop)?;
            #[cfg(not(target_os = "linux"))]
            let listener = TcpListener::from_std(std)?;
            Socket::Tcp(listener)
        } else {
            let std =
                unsafe { std::os::unix::net::UnixListener::from_raw_fd(prepared.into_raw_fd()) };
            #[cfg(target_os = "linux")]
            let listener = UnixListener::from_std(std, stop)?;
            #[cfg(not(target_os = "linux"))]
            let listener = UnixListener::from_std(std)?;
            Socket::Unix(listener)
        };
        Ok(Self {
            socket,
            addr,
            #[cfg(not(target_os = "linux"))]
            stop,
        })
    }

    /// Runs the accept loop on the calling thread until the stop handle fires. A blocked
    /// accept is what lets the kernel hand each connection to one worker. Returns the
    /// listener failure, if any. The listener is closed when this returns.
    #[cfg(target_os = "linux")]
    pub fn run(self, rt: &Handle, serve: &impl Serve) -> Option<anyhow::Error> {
        let mut fatal: Option<anyhow::Error> = None;
        // tokio::spawn and from_std reach the runtime the connections run on.
        let _guard = rt.enter();
        loop {
            match accept_blocking(&self.socket, serve) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) if is_fatal_accept(&e) => {
                    fatal = Some(anyhow!("listener failed: {e}"));
                    break;
                }
                Err(e) if is_skipped_accept(&e) => {
                    tracing::debug!(target: "net", "accept skipped on {}: {e}", self.addr);
                }
                Err(e) => {
                    tracing::warn!(target: "net", "accept failed on {}: {e}", self.addr);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        fatal
    }

    /// Runs the accept loop on rt until the stop handle fires. Returns the listener
    /// failure, if any. The listener is closed when this returns.
    #[cfg(not(target_os = "linux"))]
    pub fn run(self, rt: &Handle, serve: &impl Serve) -> Option<anyhow::Error> {
        let Self {
            socket,
            addr,
            mut stop,
        } = self;
        let mut fatal: Option<anyhow::Error> = None;
        rt.block_on(async {
            loop {
                tokio::select! {
                    biased;
                    _ = stop.wait_for(|stop| *stop) => break,
                    res = accept_connection(&socket, serve) => match res {
                        Ok(()) => {}
                        Err(e) if is_fatal_accept(&e) => {
                            fatal = Some(anyhow!("listener failed: {e}"));
                            break;
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
            }
        });
        fatal
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
fn accept_blocking(socket: &Socket, serve: &impl Serve) -> std::io::Result<bool> {
    match socket {
        Socket::Tcp(l) => match l.accept_blocking()? {
            None => return Ok(false),
            Some((stream, peer)) => {
                let stream = tokio::net::TcpStream::from_std(stream)?;
                let _ = stream.set_nodelay(true);
                serve.spawn_tcp(stream, peer);
            }
        },
        Socket::Unix(l) => match l.accept_blocking()? {
            None => return Ok(false),
            Some((stream, peer)) => {
                serve.spawn_unix(
                    tokio::net::UnixStream::from_std(stream)?,
                    peer.as_pathname(),
                );
            }
        },
    }
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
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
