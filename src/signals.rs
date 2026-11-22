use std::io;

const HOST: [libc::c_int; 6] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGHUP,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

pub struct Signals(libc::sigset_t);

impl Signals {
    /// Block host signals before creating any threads. Every thread inherits the mask. https://man7.org/linux/man-pages/man3/pthread_sigmask.3.html
    pub fn block() -> io::Result<Self> {
        // SAFETY: sigemptyset initializes set, and pthread_sigmask copies it.
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for sig in HOST {
                libc::sigaddset(&mut set, sig);
            }
            let err = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            if err != 0 {
                return Err(io::Error::from_raw_os_error(err));
            }
            Ok(Self(set))
        }
    }

    pub fn wait(&self) -> io::Result<libc::c_int> {
        loop {
            let mut sig = 0;
            // SAFETY: the set contains the blocked host signals. The main thread is their only consumer.
            let err = unsafe { libc::sigwait(&self.0, &mut sig) };
            match err {
                0 => return Ok(sig),
                libc::EINTR => continue,
                err => return Err(io::Error::from_raw_os_error(err)),
            }
        }
    }

    /// Accept a pending host signal during the drain without waiting past the stop deadline.
    pub fn pending(&self) -> io::Result<Option<libc::c_int>> {
        // SAFETY: sigpending writes a sigset_t. Only the main thread accepts these signals.
        unsafe {
            let mut pending = std::mem::zeroed();
            if libc::sigpending(&mut pending) != 0 {
                return Err(io::Error::last_os_error());
            }
            if HOST.iter().any(|s| libc::sigismember(&pending, *s) == 1) {
                return self.wait().map(Some);
            }
        }
        Ok(None)
    }
}
