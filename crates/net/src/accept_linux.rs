use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

pub type TcpListener = Listener<std::net::TcpListener>;
pub type UnixListener = Listener<std::os::unix::net::UnixListener>;

pub struct Listener<L> {
    epoll: AsyncFd<Epoll<L>>,
}

struct Epoll<L> {
    fd: OwnedFd,
    listener: L,
}

impl<L> AsRawFd for Epoll<L> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl<L: AsRawFd> Listener<L> {
    pub fn from_std(listener: L) -> io::Result<Self> {
        // SAFETY: epoll_create1 returns a new descriptor and takes no pointers.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let epoll = Epoll {
            // SAFETY: fd is a new descriptor owned by this instance.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            listener,
        };
        epoll.control(libc::EPOLL_CTL_ADD)?;
        Ok(Self {
            epoll: AsyncFd::with_interest(epoll, Interest::READABLE)?,
        })
    }

    async fn accept_with<R>(&self, mut accept: impl FnMut(&L) -> io::Result<R>) -> io::Result<R> {
        loop {
            let mut ready = self.epoll.readable().await?;
            match ready.try_io(|epoll| epoll.get_ref().wait()) {
                Ok(result) => result?,
                Err(_) => continue,
            }
            match accept(&self.epoll.get_ref().listener) {
                // Another worker can consume the connection after epoll_wait.
                // An empty wait returns WouldBlock, so try_io clears Tokio's readiness.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
    }

    pub fn on_accept(&self) -> io::Result<()> {
        // Re-registration gives other exclusive waiters a chance to accept.
        // https://github.com/nginx/nginx/blob/release-1.27.5/src/event/ngx_event_accept.c#L432-L475
        self.epoll.get_ref().control(libc::EPOLL_CTL_DEL)?;
        self.epoll.get_ref().control(libc::EPOLL_CTL_ADD)
    }
}

impl TcpListener {
    pub async fn accept(&self) -> io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
        let (stream, peer) = self.accept_with(std::net::TcpListener::accept).await?;
        stream.set_nonblocking(true)?;
        Ok((tokio::net::TcpStream::from_std(stream)?, peer))
    }
}

impl UnixListener {
    pub async fn accept(
        &self,
    ) -> io::Result<(tokio::net::UnixStream, std::os::unix::net::SocketAddr)> {
        let (stream, peer) = self
            .accept_with(std::os::unix::net::UnixListener::accept)
            .await?;
        stream.set_nonblocking(true)?;
        Ok((tokio::net::UnixStream::from_std(stream)?, peer))
    }
}

impl<L: AsRawFd> Epoll<L> {
    fn control(&self, operation: libc::c_int) -> io::Result<()> {
        // Each worker registers the shared listener in its own epoll instance.
        // https://man7.org/linux/man-pages/man2/epoll_ctl.2.html
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLEXCLUSIVE) as u32,
            u64: 0,
        };
        loop {
            // SAFETY: event is initialized and remains valid for this call.
            let result = unsafe {
                libc::epoll_ctl(
                    self.fd.as_raw_fd(),
                    operation,
                    self.listener.as_raw_fd(),
                    &mut event,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn wait(&self) -> io::Result<()> {
        let mut event = libc::epoll_event { events: 0, u64: 0 };
        loop {
            // SAFETY: event holds one result and the zero timeout cannot block.
            let count = unsafe { libc::epoll_wait(self.fd.as_raw_fd(), &mut event, 1, 0) };
            match count {
                1 => return Ok(()),
                0 => return Err(io::ErrorKind::WouldBlock.into()),
                _ => {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn stale_ready_event_waits_for_the_next_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let drainer = listener.try_clone().unwrap();
        let listener = Listener::from_std(listener).unwrap();
        let first_client = std::net::TcpStream::connect(addr).unwrap();
        let (drained_tx, drained_rx) = oneshot::channel();
        let second_client = tokio::spawn(async move {
            drained_rx.await.unwrap();
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let local_addr = stream.local_addr().unwrap();
            (stream, local_addr)
        });

        let mut drained_tx = Some(drained_tx);
        let mut drained_stream = None;
        let mut calls = 0;
        let (_, peer) = timeout(
            Duration::from_secs(2),
            listener.accept_with(|registered| {
                calls += 1;
                if let Some(tx) = drained_tx.take() {
                    drained_stream = Some(drainer.accept()?.0);
                    let result = registered.accept();
                    assert!(matches!(
                        &result,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock
                    ));
                    tx.send(()).unwrap();
                    return result;
                }
                registered.accept()
            }),
        )
        .await
        .expect("accept must wait for the second connection")
        .unwrap();
        let (second_client, second_addr) = second_client.await.unwrap();

        assert_eq!(calls, 2);
        assert!(drained_stream.is_some());
        assert_eq!(peer, second_addr);
        drop((first_client, second_client));
    }
}
