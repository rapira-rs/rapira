use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::watch;

mod listen;
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

    /// Waits for open connections to close within `grace`.
    pub async fn drain(&self, grace: Duration) -> bool {
        tokio::time::timeout(grace, self.0.closed()).await.is_ok()
    }
}

/// Starts a connection for each raw nonblocking socket that [`PreparedListener::run`] accepts.
/// Accepted sockets are not registered on the accept reactor, so callers can register them on the
/// current Tokio runtime or move them to a dedicated IO shard runtime.
pub trait Serve {
    fn spawn_tcp(&self, stream: std::net::TcpStream, peer: SocketAddr);
    fn spawn_unix(&self, stream: std::os::unix::net::UnixStream, peer: Option<PathBuf>);
}

enum RawSocket {
    Tcp(AsyncFd<std::net::TcpListener>),
    Unix(AsyncFd<std::os::unix::net::UnixListener>),
}

/// Caps one raw accept burst so the accept loop yields to check `stop` under continuous connection churn.
const ACCEPT_BATCH_LIMIT: usize = 64;

impl PreparedListener {
    /// Accepts nonblocking connections until `stop` is true or its sender closes.
    pub async fn run(self, mut stop: watch::Receiver<bool>, serve: &impl Serve) -> Result<()> {
        let Self { fd, addr } = self;
        let accept = async {
            let socket = match addr {
                ListenAddr::Tcp(_) => {
                    // SAFETY: `std::net::TcpListener` owns its descriptor for the lifetime of `AsyncFd`.
                    unsafe {
                        AsyncFd::register_with_interest(
                            std::net::TcpListener::from(fd),
                            Interest::READABLE,
                        )
                    }
                    .map(RawSocket::Tcp)
                    .map_err(std::io::Error::from)
                }
                ListenAddr::Unix(_) => {
                    // SAFETY: `std::os::unix::net::UnixListener` owns its descriptor for the lifetime of `AsyncFd`.
                    unsafe {
                        AsyncFd::register_with_interest(
                            std::os::unix::net::UnixListener::from(fd),
                            Interest::READABLE,
                        )
                    }
                    .map(RawSocket::Unix)
                    .map_err(std::io::Error::from)
                }
            }
            .with_context(|| format!("registering listener on {addr}"))?;

            loop {
                let step = match &socket {
                    RawSocket::Tcp(l) => {
                        accept_batch(l, |raw| {
                            let (stream, peer) = accept_tcp(raw)?;
                            let _ = stream.set_nodelay(true);
                            serve.spawn_tcp(stream, peer);
                            Ok(())
                        })
                        .await
                    }
                    RawSocket::Unix(l) => {
                        accept_batch(l, |raw| {
                            let (stream, peer) = accept_unix(raw)?;
                            serve.spawn_unix(stream, peer);
                            Ok(())
                        })
                        .await
                    }
                };
                match step {
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

async fn accept_batch<L, F>(l: &AsyncFd<L>, mut accept_one: F) -> std::io::Result<()>
where
    L: AsRawFd,
    F: FnMut(&L) -> std::io::Result<()>,
{
    let mut guard = l.readable().await?;
    for _ in 0..ACCEPT_BATCH_LIMIT {
        match guard.try_io(|inner| accept_one(inner.get_ref())) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => return Ok(()),
        }
    }
    // Yield after a full burst so `select!` can observe `stop`.
    tokio::task::yield_now().await;
    Ok(())
}

/// Accepts one socket with `SOCK_NONBLOCK | SOCK_CLOEXEC` in one syscall on Linux.
/// https://man7.org/linux/man-pages/man2/accept4.2.html
#[cfg(target_os = "linux")]
unsafe fn accept4_raw(
    listener_fd: std::os::fd::RawFd,
    addr: *mut libc::sockaddr,
    len: *mut libc::socklen_t,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};

    // SAFETY: Caller guarantees `listener_fd` is a valid listening socket and `addr`/`len` are valid.
    let fd = unsafe {
        libc::accept4(
            listener_fd,
            addr,
            len,
            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `accept4` returned a fresh owned socket descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn accept_tcp(l: &std::net::TcpListener) -> std::io::Result<(std::net::TcpStream, SocketAddr)> {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `l` owns a valid listening socket descriptor; `storage` and `len` are writable.
    let fd = unsafe {
        accept4_raw(
            l.as_raw_fd(),
            (&raw mut storage).cast::<libc::sockaddr>(),
            &mut len,
        )
    }?;
    let peer = sockaddr_to_std(&storage, len)?;
    Ok((std::net::TcpStream::from(fd), peer))
}

#[cfg(target_os = "linux")]
fn sockaddr_to_std(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> std::io::Result<SocketAddr> {
    let len = len as usize;
    match i32::from(storage.ss_family) {
        libc::AF_INET if len >= std::mem::size_of::<libc::sockaddr_in>() => {
            // SAFETY: `ss_family == AF_INET` and `len` covers `sockaddr_in`.
            let sin =
                unsafe { &*(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Ok(SocketAddr::new(IpAddr::V4(ip), port))
        }
        libc::AF_INET6 if len >= std::mem::size_of::<libc::sockaddr_in6>() => {
            // SAFETY: `ss_family == AF_INET6` and `len` covers `sockaddr_in6`.
            let sin6 = unsafe {
                &*(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>()
            };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        _ => Err(std::io::Error::from_raw_os_error(libc::EAFNOSUPPORT)),
    }
}

#[cfg(not(target_os = "linux"))]
fn accept_tcp(l: &std::net::TcpListener) -> std::io::Result<(std::net::TcpStream, SocketAddr)> {
    let (stream, peer) = l.accept()?;
    stream.set_nonblocking(true)?;
    Ok((stream, peer))
}

#[cfg(target_os = "linux")]
fn accept_unix(
    l: &std::os::unix::net::UnixListener,
) -> std::io::Result<(std::os::unix::net::UnixStream, Option<PathBuf>)> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut storage: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: `l` owns a valid listening unix socket descriptor; `storage` and `len` are writable.
    let fd = unsafe {
        accept4_raw(
            l.as_raw_fd(),
            (&raw mut storage).cast::<libc::sockaddr>(),
            &mut len,
        )
    }?;
    let stream = std::os::unix::net::UnixStream::from(fd);
    let path_offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    let peer = if (len as usize) > path_offset {
        let raw_len = ((len as usize) - path_offset).min(storage.sun_path.len());
        // SAFETY: `sun_path` is a contiguous byte array of `c_char`.
        let bytes =
            unsafe { std::slice::from_raw_parts(storage.sun_path.as_ptr().cast::<u8>(), raw_len) };
        // Abstract sockets start with NUL and carry no filesystem path.
        if bytes.first() == Some(&0) {
            None
        } else {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            (!bytes[..end].is_empty()).then(|| PathBuf::from(OsStr::from_bytes(&bytes[..end])))
        }
    } else {
        None
    };
    Ok((stream, peer))
}

#[cfg(not(target_os = "linux"))]
fn accept_unix(
    l: &std::os::unix::net::UnixListener,
) -> std::io::Result<(std::os::unix::net::UnixStream, Option<PathBuf>)> {
    let (stream, peer) = l.accept()?;
    stream.set_nonblocking(true)?;
    let path = peer.as_pathname().map(std::path::Path::to_path_buf);
    Ok((stream, path))
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
