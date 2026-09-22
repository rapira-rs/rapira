use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

pub(super) type TcpListener = Listener<std::net::TcpListener>;
pub(super) type UnixListener = Listener<std::os::unix::net::UnixListener>;

// epoll_event.u64 tags, one per registered descriptor.
const LISTENER_TAG: u64 = 0;
const WAKE_TAG: u64 = 1;

/// Stops the accept loop of one worker. The loop waits on the same eventfd that every
/// holder of this handle writes.
#[derive(Clone)]
pub(super) struct Wake(Arc<OwnedFd>);

pub(super) struct Listener<L> {
    epoll: OwnedFd,
    listener: L,
    wake: Wake,
}

impl Wake {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: eventfd takes no pointers and returns a new descriptor.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a new descriptor owned by this handle.
        Ok(Self(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    pub(super) fn handle(&self) -> Self {
        self.clone()
    }

    /// Makes the current or next [`Listener::accept_blocking`] return `None`.
    pub(super) fn stop(&self) {
        let value: u64 = 1;
        // SAFETY: an eventfd write reads exactly the eight bytes of value.
        let written = unsafe { libc::write(self.0.as_raw_fd(), (&raw const value).cast(), 8) };
        // Nothing reads the counter, and an add of 1 fails only when the counter would overflow.
        // https://man7.org/linux/man-pages/man2/eventfd.2.html
        debug_assert_eq!(written, 8);
    }
}

impl<L: AsRawFd> Listener<L> {
    pub(super) fn from_std(listener: L, wake: Wake) -> io::Result<Self> {
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
                // which the accept loop treats as fatal.
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
        loop {
            // SAFETY: event is initialized and remains valid for this call.
            let result =
                unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), operation, fd, &mut event) };
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
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
    pub(super) fn accept_blocking(
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
    pub(super) fn accept_blocking(
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);
    /// Long enough for every spawned loop to reach epoll_wait.
    const SETTLE: Duration = Duration::from_millis(100);

    fn assert_nonblocking(stream: &impl AsRawFd) {
        // SAFETY: fcntl reads flags from the live stream descriptor.
        let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
    }

    /// One accept loop, as a worker runs it: every accepted stream stays open and the
    /// worker id goes to `tx` in accept order.
    fn spawn_accept_loop<S: Send + 'static, A: Send + 'static>(
        id: usize,
        tx: mpsc::Sender<usize>,
        mut accept: impl FnMut() -> io::Result<Option<(S, A)>> + Send + 'static,
    ) -> JoinHandle<Vec<S>> {
        std::thread::spawn(move || {
            let mut streams = Vec::new();
            while let Some((stream, _)) = accept().unwrap() {
                streams.push(stream);
                tx.send(id).unwrap();
            }
            streams
        })
    }

    /// Two epoll instances on one listening socket stand in for two workers: each
    /// registers its own exclusive entry on the socket wait queue.
    #[test]
    fn exclusive_wake_alternates_between_listeners() {
        let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        bound.set_nonblocking(true).unwrap();
        let addr = bound.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut wakes = Vec::new();
        let mut loops = Vec::new();
        for id in 0..2 {
            let wake = Wake::new().unwrap();
            let listener = TcpListener::from_std(bound.try_clone().unwrap(), wake.clone()).unwrap();
            wakes.push(wake);
            loops.push(spawn_accept_loop(id, tx.clone(), move || {
                listener.accept_blocking()
            }));
        }
        std::thread::sleep(SETTLE);

        let mut clients = Vec::new();
        let mut order = Vec::new();
        for _ in 0..4 {
            clients.push(std::net::TcpStream::connect(addr).unwrap());
            order.push(rx.recv_timeout(WAIT).unwrap());
        }
        for wake in &wakes {
            wake.stop();
        }
        for handle in loops {
            handle.join().unwrap();
        }

        let first = order[0];
        assert_eq!(
            order,
            vec![first, 1 - first, first, 1 - first],
            "one connection must wake one worker, and the worker that accepted must go last"
        );
    }

    #[test]
    fn unix_listener_accepts_a_nonblocking_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http.sock");
        let bound = std::os::unix::net::UnixListener::bind(&path).unwrap();
        bound.set_nonblocking(true).unwrap();
        let listener = UnixListener::from_std(bound, Wake::new().unwrap()).unwrap();
        let mut client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        client.write_all(b"x").unwrap();

        let (mut stream, _) = listener.accept_blocking().unwrap().unwrap();
        assert_nonblocking(&stream);
        stream.set_nonblocking(false).unwrap();
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
    }

    #[test]
    fn a_wake_stops_the_loop() {
        let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        bound.set_nonblocking(true).unwrap();
        let wake = Wake::new().unwrap();
        let listener = TcpListener::from_std(bound, wake.handle()).unwrap();

        wake.stop();
        assert!(listener.accept_blocking().unwrap().is_none());
    }

    /// Re-registration must not lose the connections already queued on the socket.
    #[test]
    fn queued_connections_survive_each_rotation() {
        let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        bound.set_nonblocking(true).unwrap();
        let addr = bound.local_addr().unwrap();
        let mut clients = Vec::new();
        for byte in 0..16_u8 {
            let mut client = std::net::TcpStream::connect(addr).unwrap();
            client.write_all(&[byte]).unwrap();
            clients.push(client);
        }
        let listener = TcpListener::from_std(bound, Wake::new().unwrap()).unwrap();

        let mut received = std::collections::BTreeSet::new();
        for _ in 0..16 {
            let (mut stream, peer) = listener
                .accept_blocking()
                .unwrap()
                .expect("a queued connection must not turn into a shutdown");
            assert!(
                clients
                    .iter()
                    .any(|client| client.local_addr().unwrap() == peer)
            );
            assert_nonblocking(&stream);
            stream.set_nonblocking(false).unwrap();
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).unwrap();
            received.insert(byte[0]);
        }
        assert_eq!(received, (0..16).collect());
    }

    /// A worker passed over by the exclusive walk finds an empty accept queue.
    #[test]
    fn a_stolen_connection_waits_for_the_next_one() {
        let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        bound.set_nonblocking(true).unwrap();
        let addr = bound.local_addr().unwrap();
        let thief = bound.try_clone().unwrap();
        let listener = TcpListener::from_std(bound, Wake::new().unwrap()).unwrap();
        let first_client = std::net::TcpStream::connect(addr).unwrap();
        let (stolen_tx, stolen_rx) = mpsc::channel();
        let second_client = std::thread::spawn(move || {
            stolen_rx.recv().unwrap();
            std::net::TcpStream::connect(addr).unwrap()
        });

        let mut stolen_tx = Some(stolen_tx);
        let mut stolen_stream = None;
        let mut calls = 0;
        let accepted = listener
            .accept_blocking_with(|registered| {
                calls += 1;
                if let Some(tx) = stolen_tx.take() {
                    stolen_stream = Some(thief.accept()?.0);
                    let result = registered.accept();
                    assert!(matches!(
                        &result,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock
                    ));
                    tx.send(()).unwrap();
                    return result;
                }
                registered.accept()
            })
            .unwrap();
        let second_client = second_client.join().unwrap();

        let (_, peer) = accepted.expect("the loop must wait for the next connection");
        assert_eq!(calls, 2);
        assert!(stolen_stream.is_some());
        assert_eq!(peer, second_client.local_addr().unwrap());
        drop(first_client);
    }

    /// A listener whose accept failed must still move behind the others, so the next
    /// wake goes to a worker that can accept.
    #[test]
    fn a_failed_accept_moves_the_listener_behind_the_others() {
        let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        bound.set_nonblocking(true).unwrap();
        let addr = bound.local_addr().unwrap();
        let thief = bound.try_clone().unwrap();
        // Registration order is queue order: the failing listener starts at the head.
        let failing_wake = Wake::new().unwrap();
        let failing =
            TcpListener::from_std(bound.try_clone().unwrap(), failing_wake.clone()).unwrap();
        let other_wake = Wake::new().unwrap();
        let other = TcpListener::from_std(bound, other_wake.clone()).unwrap();
        let (tx, rx) = mpsc::channel();
        let (failed_tx, failed_rx) = mpsc::channel();
        let failing_loop = std::thread::spawn({
            let tx = tx.clone();
            move || {
                // The first wake takes the connection away and fails as a worker at its
                // descriptor limit does, so the listener has nothing to accept afterwards.
                let error = failing
                    .accept_blocking_with(
                        |_| -> io::Result<(std::net::TcpStream, std::net::SocketAddr)> {
                            drop(thief.accept()?);
                            Err(io::Error::from_raw_os_error(libc::EMFILE))
                        },
                    )
                    .unwrap_err();
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                failed_tx.send(()).unwrap();
                let mut streams = Vec::new();
                while let Some((stream, _)) = failing.accept_blocking().unwrap() {
                    streams.push(stream);
                    tx.send(0).unwrap();
                }
                streams
            }
        });
        let other_loop = spawn_accept_loop(1, tx, move || other.accept_blocking());
        std::thread::sleep(SETTLE);

        let first_client = std::net::TcpStream::connect(addr).unwrap();
        failed_rx.recv_timeout(WAIT).unwrap();
        std::thread::sleep(SETTLE);
        let second_client = std::net::TcpStream::connect(addr).unwrap();
        assert_eq!(
            rx.recv_timeout(WAIT).unwrap(),
            1,
            "the wake must skip the listener that failed its accept"
        );

        failing_wake.stop();
        other_wake.stop();
        failing_loop.join().unwrap();
        other_loop.join().unwrap();
        drop((first_client, second_client));
    }
}
