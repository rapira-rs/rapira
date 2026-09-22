use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::handler::AcceptGate;

pub(super) type TcpListener = Listener<std::net::TcpListener>;
pub(super) type UnixListener = Listener<std::os::unix::net::UnixListener>;

// epoll_event.u64 tags, one per registered descriptor.
const LISTENER_TAG: u64 = 0;
const WAKE_TAG: u64 = 1;

/// Stops the accept loop of one worker. The loop waits on the same eventfd that every
/// holder of this handle writes.
#[derive(Clone)]
pub(super) struct Wake(Arc<OwnedFd>);

/// One result of [`Listener::accept_blocking`].
pub(super) enum Accepted<S, A> {
    Stream(S, A),
    Shutdown,
}

pub(super) struct Listener<L> {
    inner: Arc<Inner<L>>,
}

struct Inner<L> {
    epoll: OwnedFd,
    listener: L,
    wake: Wake,
    state: Mutex<Registration>,
}

struct Registration {
    /// The listener sits in this epoll.
    registered: bool,
    /// The worker is busy and takes no new connection.
    gated: bool,
}

/// The descriptors one [`Inner::wait`] call reports.
struct Ready {
    listener: bool,
    wake: bool,
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

    /// Makes the current or next [`Listener::accept_blocking`] return [`Accepted::Shutdown`].
    pub(super) fn wake(&self) -> io::Result<()> {
        let value: u64 = 1;
        // SAFETY: an eventfd write reads exactly the eight bytes of value.
        let written = unsafe { libc::write(self.0.as_raw_fd(), (&raw const value).cast(), 8) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Clears the counter so the next wait blocks again. An empty eventfd reads EAGAIN.
    fn drain(&self) {
        let mut value: u64 = 0;
        // SAFETY: an eventfd read writes exactly the eight bytes of value.
        let _ = unsafe { libc::read(self.0.as_raw_fd(), (&raw mut value).cast(), 8) };
    }
}

impl<L: AsRawFd> Listener<L> {
    pub(super) fn from_std(listener: L, wake: Wake) -> io::Result<Self> {
        // SAFETY: epoll_create1 returns a new descriptor and takes no pointers.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let inner = Inner {
            // SAFETY: fd is a new descriptor owned by this instance.
            epoll: unsafe { OwnedFd::from_raw_fd(fd) },
            listener,
            wake,
            state: Mutex::new(Registration {
                registered: false,
                gated: false,
            }),
        };
        inner.control(
            libc::EPOLL_CTL_ADD,
            inner.wake.0.as_raw_fd(),
            libc::EPOLLIN as u32,
            WAKE_TAG,
        )?;
        inner.apply(&mut inner.state())?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Blocks until this worker owns a connection or the wake descriptor fires.
    fn accept_blocking_with<S, A>(
        &self,
        mut accept: impl FnMut(&L) -> io::Result<(S, A)>,
    ) -> io::Result<Accepted<S, A>> {
        loop {
            let ready = self.inner.wait()?;
            if ready.wake {
                self.inner.wake.drain();
                return Ok(Accepted::Shutdown);
            }
            if !ready.listener {
                continue;
            }
            match accept(&self.inner.listener) {
                Ok((stream, peer)) => {
                    self.inner.rotate()?;
                    return Ok(Accepted::Stream(stream, peer));
                }
                // Another worker can take the connection before this accept runs.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl<L: AsRawFd + Send + Sync + 'static> Listener<L> {
    /// The handle that stops and restarts accepting while the loop runs.
    pub(super) fn gate(&self) -> Arc<dyn AcceptGate> {
        self.inner.clone()
    }
}

impl TcpListener {
    pub(super) fn accept_blocking(
        &self,
    ) -> io::Result<Accepted<std::net::TcpStream, std::net::SocketAddr>> {
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
    ) -> io::Result<Accepted<std::os::unix::net::UnixStream, std::os::unix::net::SocketAddr>> {
        self.accept_blocking_with(|listener| {
            let (stream, peer) = listener.accept()?;
            stream.set_nonblocking(true)?;
            Ok((stream, peer))
        })
    }
}

impl<L: AsRawFd + Send + Sync> AcceptGate for Inner<L> {
    fn set_busy(&self, busy: bool) -> io::Result<()> {
        let mut state = self.state();
        state.gated = busy;
        self.apply(&mut state)
    }
}

impl<L: AsRawFd> Inner<L> {
    fn state(&self) -> MutexGuard<'_, Registration> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// Holds the registration the gate asks for.
    fn apply(&self, state: &mut Registration) -> io::Result<()> {
        if state.gated && state.registered {
            self.listener_control(libc::EPOLL_CTL_DEL)?;
            state.registered = false;
        } else if !state.gated && !state.registered {
            self.listener_control(libc::EPOLL_CTL_ADD)?;
            state.registered = true;
        }
        Ok(())
    }

    /// Re-registration moves this worker to the tail of the wait queue, so the next
    /// connection reaches another worker first. EPOLL_CTL_MOD rejects an exclusive
    /// item, so the item is removed and added again.
    /// https://github.com/nginx/nginx/blob/release-1.27.5/src/event/ngx_event_accept.c#L432-L475
    fn rotate(&self) -> io::Result<()> {
        let mut state = self.state();
        if state.registered {
            self.listener_control(libc::EPOLL_CTL_DEL)?;
            state.registered = false;
        }
        self.apply(&mut state)
    }

    fn wait(&self) -> io::Result<Ready> {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 2];
        loop {
            // SAFETY: events holds one slot per registered descriptor.
            let count =
                unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), 2, -1) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
                continue;
            }
            let mut ready = Ready {
                listener: false,
                wake: false,
            };
            for event in &events[..count as usize] {
                match event.u64 {
                    WAKE_TAG => ready.wake = true,
                    _ => ready.listener = true,
                }
            }
            return Ok(ready);
        }
    }
}

#[cfg(test)]
mod tests {
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
    fn spawn_accept_loop<S: Send + 'static>(
        id: usize,
        tx: mpsc::Sender<usize>,
        mut accept: impl FnMut() -> io::Result<Option<S>> + Send + 'static,
    ) -> JoinHandle<Vec<S>> {
        std::thread::spawn(move || {
            let mut streams = Vec::new();
            while let Some(stream) = accept().unwrap() {
                streams.push(stream);
                tx.send(id).unwrap();
            }
            streams
        })
    }

    fn next_tcp(listener: &TcpListener) -> io::Result<Option<std::net::TcpStream>> {
        Ok(match listener.accept_blocking()? {
            Accepted::Stream(stream, _) => Some(stream),
            Accepted::Shutdown => None,
        })
    }

    fn next_unix(listener: &UnixListener) -> io::Result<Option<std::os::unix::net::UnixStream>> {
        Ok(match listener.accept_blocking()? {
            Accepted::Stream(stream, _) => Some(stream),
            Accepted::Shutdown => None,
        })
    }

    fn stop<S>(wakes: &[Wake], loops: Vec<JoinHandle<Vec<S>>>) {
        for wake in wakes {
            wake.wake().unwrap();
        }
        for handle in loops {
            handle.join().unwrap();
        }
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
                next_tcp(&listener)
            }));
        }
        std::thread::sleep(SETTLE);

        let mut clients = Vec::new();
        let mut order = Vec::new();
        for _ in 0..4 {
            clients.push(std::net::TcpStream::connect(addr).unwrap());
            order.push(rx.recv_timeout(WAIT).unwrap());
        }
        stop(&wakes, loops);

        let first = order[0];
        assert_eq!(
            order,
            vec![first, 1 - first, first, 1 - first],
            "one connection must wake one worker, and the worker that accepted must go last"
        );
    }

    #[test]
    fn exclusive_wake_alternates_between_unix_listeners() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http.sock");
        let bound = std::os::unix::net::UnixListener::bind(&path).unwrap();
        bound.set_nonblocking(true).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut wakes = Vec::new();
        let mut loops = Vec::new();
        for id in 0..2 {
            let wake = Wake::new().unwrap();
            let listener =
                UnixListener::from_std(bound.try_clone().unwrap(), wake.clone()).unwrap();
            wakes.push(wake);
            loops.push(spawn_accept_loop(id, tx.clone(), move || {
                next_unix(&listener)
            }));
        }
        std::thread::sleep(SETTLE);

        let mut clients = Vec::new();
        let mut order = Vec::new();
        for _ in 0..4 {
            clients.push(std::os::unix::net::UnixStream::connect(&path).unwrap());
            order.push(rx.recv_timeout(WAIT).unwrap());
        }
        stop(&wakes, loops);

        let first = order[0];
        assert_eq!(order, vec![first, 1 - first, first, 1 - first]);
    }

    #[test]
    fn a_busy_listener_leaves_the_connections_to_the_idle_one() {
        let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        bound.set_nonblocking(true).unwrap();
        let addr = bound.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut wakes = Vec::new();
        let mut gates = Vec::new();
        let mut loops = Vec::new();
        for id in 0..2 {
            let wake = Wake::new().unwrap();
            let listener = TcpListener::from_std(bound.try_clone().unwrap(), wake.clone()).unwrap();
            wakes.push(wake);
            gates.push(listener.gate());
            loops.push(spawn_accept_loop(id, tx.clone(), move || {
                next_tcp(&listener)
            }));
        }
        std::thread::sleep(SETTLE);

        gates[0].set_busy(true).unwrap();
        let mut clients = Vec::new();
        let mut order = Vec::new();
        for _ in 0..4 {
            clients.push(std::net::TcpStream::connect(addr).unwrap());
            order.push(rx.recv_timeout(WAIT).unwrap());
        }
        assert_eq!(order, vec![1, 1, 1, 1], "a busy worker must not accept");

        gates[0].set_busy(false).unwrap();
        let mut resumed = Vec::new();
        for _ in 0..2 {
            clients.push(std::net::TcpStream::connect(addr).unwrap());
            resumed.push(rx.recv_timeout(WAIT).unwrap());
        }
        stop(&wakes, loops);

        assert!(
            resumed.contains(&0),
            "an idle worker must take part again: {resumed:?}"
        );
    }

    #[test]
    fn a_wake_stops_the_loop_and_leaves_it_usable() {
        let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        bound.set_nonblocking(true).unwrap();
        let addr = bound.local_addr().unwrap();
        let wake = Wake::new().unwrap();
        let listener = TcpListener::from_std(bound, wake.clone()).unwrap();

        wake.wake().unwrap();
        assert!(matches!(
            listener.accept_blocking().unwrap(),
            Accepted::Shutdown
        ));

        let client = std::net::TcpStream::connect(addr).unwrap();
        let Accepted::Stream(stream, peer) = listener.accept_blocking().unwrap() else {
            panic!("a drained wake must not stop the loop again")
        };
        assert_eq!(peer, client.local_addr().unwrap());
        assert_nonblocking(&stream);
    }

    /// Re-registration must not lose the connections already queued on the socket.
    #[test]
    fn queued_connections_survive_each_rotation() {
        use std::io::{Read, Write};

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
            let Accepted::Stream(mut stream, peer) = listener.accept_blocking().unwrap() else {
                panic!("a queued connection must not turn into a shutdown")
            };
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

        let Accepted::Stream(_, peer) = accepted else {
            panic!("the loop must wait for the next connection")
        };
        assert_eq!(calls, 2);
        assert!(stolen_stream.is_some());
        assert_eq!(peer, second_client.local_addr().unwrap());
        drop(first_client);
    }
}
