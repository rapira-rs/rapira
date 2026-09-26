use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

pub(crate) type TcpListener = Listener<std::net::TcpListener>;
pub(crate) type UnixListener = Listener<std::os::unix::net::UnixListener>;

// epoll_event.u64 tags, one per registered descriptor.
const LISTENER_TAG: u64 = 0;
const WAKE_TAG: u64 = 1;

/// Stops the accept loop of one worker. The loop waits on the same eventfd that every
/// holder of this handle writes.
#[derive(Clone)]
pub struct Wake(Arc<OwnedFd>);

pub(crate) struct Listener<L> {
    epoll: OwnedFd,
    listener: L,
    wake: Wake,
}

impl Wake {
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd takes no pointers and returns a new descriptor.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a new descriptor owned by this handle.
        Ok(Self(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// Makes the current or next [`Listener::accept_blocking`] return `None`.
    pub fn stop(&self) {
        let value: u64 = 1;
        // SAFETY: an eventfd write reads exactly the eight bytes of value.
        let written = unsafe { libc::write(self.0.as_raw_fd(), (&raw const value).cast(), 8) };
        // Nothing reads the counter, and an add of 1 fails only when the counter would overflow.
        // https://man7.org/linux/man-pages/man2/eventfd.2.html
        debug_assert_eq!(written, 8);
    }
}

impl<L: AsRawFd> Listener<L> {
    pub(crate) fn from_std(listener: L, wake: Wake) -> io::Result<Self> {
        // SAFETY: epoll_create1 returns a new descriptor and takes no pointers.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let listener = Self {
            // SAFETY: fd is a new descriptor owned by this instance.
            epoll: unsafe { OwnedFd::from_raw_fd(fd) },
            listener,
            wake,
        };
        listener.control(
            libc::EPOLL_CTL_ADD,
            listener.wake.0.as_raw_fd(),
            libc::EPOLLIN as u32,
            WAKE_TAG,
        )?;
        listener.listener_control(libc::EPOLL_CTL_ADD)?;
        Ok(listener)
    }

    /// Blocks until this worker owns a connection. `None` means the wake descriptor fired.
    fn accept_blocking_with<S, A>(
        &self,
        mut accept: impl FnMut(&L) -> io::Result<(S, A)>,
    ) -> io::Result<Option<(S, A)>> {
        loop {
            if self.wait()? {
                return Ok(None);
            }
            match accept(&self.listener) {
                // Another worker can take the connection before this accept runs.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                // Every other result moves this worker behind the others. A rotation failure
                // leaves the listener unregistered, so it is wrapped as `ErrorKind::Other`,
                // which the accept loop treats as fatal. The stream accepted in that call closes:
                // for a valid listener only ENOMEM or ENOSPC fail the rotation, and these can
                // also fail the tokio registration of the stream.
                result => {
                    self.rotate().map_err(io::Error::other)?;
                    return result.map(Some);
                }
            }
        }
    }

    fn control(&self, operation: libc::c_int, fd: RawFd, events: u32, tag: u64) -> io::Result<()> {
        // Each worker registers the shared listener in its own epoll instance.
        // https://man7.org/linux/man-pages/man2/epoll_ctl.2.html
        let mut event = libc::epoll_event { events, u64: tag };
        // SAFETY: event is initialized and remains valid for this call.
        if unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), operation, fd, &mut event) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// A thread blocked in epoll_wait is what makes EPOLLEXCLUSIVE hand one connection
    /// to one worker: the kernel walks the socket wait queue and stops at the first
    /// waiter it wakes.
    fn listener_control(&self, operation: libc::c_int) -> io::Result<()> {
        self.control(
            operation,
            self.listener.as_raw_fd(),
            (libc::EPOLLIN | libc::EPOLLEXCLUSIVE) as u32,
            LISTENER_TAG,
        )
    }

    /// Re-registration moves this worker to the tail of the wait queue, so the next
    /// connection reaches another worker first. EPOLL_CTL_MOD rejects an exclusive
    /// item, so the item is removed and added again.
    /// https://github.com/nginx/nginx/blob/release-1.27.5/src/event/ngx_event_accept.c#L432-L475
    fn rotate(&self) -> io::Result<()> {
        self.listener_control(libc::EPOLL_CTL_DEL)?;
        self.listener_control(libc::EPOLL_CTL_ADD)
    }

    /// Blocks until a registered descriptor is ready. True means the wake descriptor fired.
    fn wait(&self) -> io::Result<bool> {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 2];
        loop {
            // SAFETY: events holds one slot per registered descriptor.
            let count =
                unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), 2, -1) };
            if count >= 0 {
                return Ok(events[..count as usize]
                    .iter()
                    .any(|event| event.u64 == WAKE_TAG));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl TcpListener {
    pub(crate) fn accept_blocking(
        &self,
    ) -> io::Result<Option<(std::net::TcpStream, std::net::SocketAddr)>> {
        self.accept_blocking_with(|listener| {
            let (stream, peer) = listener.accept()?;
            stream.set_nonblocking(true)?;
            Ok((stream, peer))
        })
    }
}

impl UnixListener {
    pub(crate) fn accept_blocking(
        &self,
    ) -> io::Result<
        Option<(
            std::os::unix::net::UnixStream,
            std::os::unix::net::SocketAddr,
        )>,
    > {
        self.accept_blocking_with(|listener| {
            let (stream, peer) = listener.accept()?;
            stream.set_nonblocking(true)?;
            Ok((stream, peer))
        })
    }
}
