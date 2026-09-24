use std::net::SocketAddr;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// The backlog of every master-bound listener. The master binds before the fork, and the workers inherit the queue. The kernel caps the value at `net.core.somaxconn`. https://man7.org/linux/man-pages/man2/listen.2.html
const LISTEN_BACKLOG: i32 = 65535;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddr {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl std::fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(a) => write!(f, "{a}"),
            Self::Unix(p) => write!(f, "unix:{}", p.display()),
        }
    }
}

/// Exactly one closer per process: the master holds its copy for its whole life so respawned workers keep inheriting it, a worker hands its copy to the adopter.
#[derive(Debug)]
pub struct PreparedListener {
    fd: OwnedFd,
    addr: ListenAddr,
}

impl PreparedListener {
    pub fn addr(&self) -> &ListenAddr {
        &self.addr
    }
}

impl IntoRawFd for PreparedListener {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

/// Runs before any fork and before a runtime exists: sync syscalls only, one context per boot.
#[derive(Default)]
pub struct PrepareCtx {
    fds: Vec<OwnedFd>,
}

impl PrepareCtx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Backed by dups owned by this context, so the fds stay valid even if an extension drops its `PreparedListener`.
    pub fn listener_fds(&self) -> Vec<RawFd> {
        self.fds.iter().map(|fd| fd.as_raw_fd()).collect()
    }

    /// Sets O_NONBLOCK: tokio's `from_std` requires it, and the Linux acceptor needs `accept` to return WouldBlock when another worker takes the connection.
    pub fn bind_tcp(&mut self, addr: SocketAddr) -> anyhow::Result<PreparedListener> {
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
        let dup = socket.try_clone().context("dup listener fd")?;
        self.fds.push(dup.into());
        Ok(PreparedListener {
            fd: socket.into(),
            addr,
        })
    }

    /// The connect probe guards against unlinking a live socket: WouldBlock means a full backlog on a live peer, not an absent one. Mode 0o666 because the containing directory is the real access gate and an unprivileged client (the reverse proxy in front of this process) must be able to connect without uid/gid matching.
    pub fn bind_unix(&mut self, path: &Path) -> anyhow::Result<PreparedListener> {
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
        let dup = socket.try_clone().context("dup listener fd")?;
        self.fds.push(dup.into());
        Ok(PreparedListener {
            fd: socket.into(),
            addr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn tcp_bind_resolves_and_accepts() {
        let mut ctx = PrepareCtx::new();
        let l = ctx.bind_tcp("127.0.0.1:0".parse().unwrap()).unwrap();
        let ListenAddr::Tcp(resolved) = *l.addr() else {
            panic!()
        };
        assert_ne!(resolved.port(), 0);

        let fd = l.into_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags & libc::O_NONBLOCK != 0, "O_NONBLOCK expected");
        let fdflags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(fdflags & libc::FD_CLOEXEC != 0, "FD_CLOEXEC expected");

        let mut client = std::net::TcpStream::connect(resolved).unwrap();
        let std_l: std::net::TcpListener = {
            use std::os::fd::FromRawFd;
            unsafe { std::net::TcpListener::from_raw_fd(fd) }
        };
        std_l.set_nonblocking(false).unwrap();
        let (mut srv, _) = std_l.accept().unwrap();
        client.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        srv.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"x");
    }

    #[test]
    fn unix_bind_sets_perms_and_reclaims_stale() {
        let dir = std::env::temp_dir().join(format!("rapira-prep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sock");

        for _ in 0..2 {
            let mut ctx = PrepareCtx::new();
            let l = ctx.bind_unix(&path).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o666);
            drop(l);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unix_bind_refuses_live_socket() {
        let dir = std::env::temp_dir().join(format!("rapira-prep-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("live.sock");

        let mut ctx = PrepareCtx::new();
        let _live = ctx.bind_unix(&path).unwrap();
        let mut second = PrepareCtx::new();
        let err = second.bind_unix(&path).unwrap_err();
        assert!(err.to_string().contains("already listening"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_binds_rejected() {
        let mut ctx = PrepareCtx::new();
        let l = ctx.bind_tcp("127.0.0.1:0".parse().unwrap()).unwrap();
        let ListenAddr::Tcp(resolved) = *l.addr() else {
            panic!()
        };
        assert!(ctx.bind_tcp(resolved).is_err());
    }
}
