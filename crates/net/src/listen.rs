use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

pub use rapira_config::ListenAddr;

/// Linux limits the backlog to `net.core.somaxconn`. https://man7.org/linux/man-pages/man2/listen.2.html
const LISTEN_BACKLOG: i32 = 65535;

/// A listening socket and its bound address.
#[derive(Debug)]
pub struct PreparedListener {
    pub(crate) fd: OwnedFd,
    pub(crate) addr: ListenAddr,
}

impl PreparedListener {
    pub fn addr(&self) -> &ListenAddr {
        &self.addr
    }
}

/// Binds a nonblocking listener before the IO runtime starts.
pub fn bind(addr: &ListenAddr) -> anyhow::Result<PreparedListener> {
    match addr {
        ListenAddr::Tcp(addr) => bind_tcp(*addr),
        ListenAddr::Unix(path) => bind_unix(path),
    }
}

fn bind_tcp(addr: SocketAddr) -> anyhow::Result<PreparedListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .with_context(|| format!("socket for {addr}"))?;
    socket.set_reuse_address(true)?;
    socket
        .bind(&addr.into())
        .with_context(|| format!("bind {addr}"))?;
    socket
        .listen(LISTEN_BACKLOG)
        .with_context(|| format!("listen {addr}"))?;
    socket.set_nonblocking(true)?;
    let resolved = socket
        .local_addr()?
        .as_socket()
        .expect("inet socket has an inet local addr");
    let addr = ListenAddr::Tcp(resolved);
    Ok(PreparedListener {
        fd: socket.into(),
        addr,
    })
}

/// The connect probe prevents the unlink of a live socket. WouldBlock means a full backlog on a live peer, not an absent one. The mode is 0o666, because the directory that contains the socket controls the access, and an unprivileged client must be able to connect without a uid or gid match. The reverse proxy in front of this process is such a client.
fn bind_unix(path: &Path) -> anyhow::Result<PreparedListener> {
    let probe = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    probe.set_nonblocking(true)?;
    match probe.connect(&SockAddr::unix(path)?) {
        Ok(()) => {
            anyhow::bail!("another server is already listening on {}", path.display())
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            anyhow::bail!("another server is already listening on {}", path.display())
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) => {}
        Err(e) => {
            return Err(e).with_context(|| format!("probing {}", path.display()));
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("removing stale socket {}", path.display()));
        }
    }
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket
        .bind(&SockAddr::unix(path)?)
        .with_context(|| format!("bind unix:{}", path.display()))?;
    socket.listen(LISTEN_BACKLOG)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    socket.set_nonblocking(true)?;
    let addr = ListenAddr::Unix(path.to_owned());
    Ok(PreparedListener {
        fd: socket.into(),
        addr,
    })
}
