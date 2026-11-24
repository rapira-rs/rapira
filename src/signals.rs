use std::io;
use std::sync::OnceLock;

const HOST: [libc::c_int; 6] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGHUP,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

/// The main thread accepts the host signals.
static MAIN: OnceLock<libc::pthread_t> = OnceLock::new();

/// A PHP thread blocks no signal, so a host signal can arrive there. The handler passes the signal to the main thread. `pthread_kill` is async-signal-safe. https://man7.org/linux/man-pages/man7/signal-safety.7.html
extern "C" fn forward(sig: libc::c_int) {
    if let Some(main) = MAIN.get() {
        // SAFETY: the main thread lives until the process exits.
        unsafe { libc::pthread_kill(*main, sig) };
    }
}

pub struct Signals(libc::sigset_t);

impl Signals {
    /// Blocks the host signals on the main thread before it creates any thread, and installs the handler for them. Each new thread inherits the mask. https://man7.org/linux/man-pages/man3/pthread_sigmask.3.html
    pub fn block() -> io::Result<Self> {
        // SAFETY: sigemptyset initializes both sets. sigaction and pthread_sigmask copy their arguments.
        unsafe {
            MAIN.get_or_init(|| libc::pthread_self());
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward as *const () as libc::sighandler_t;
            // An interrupted system call of a PHP thread starts again.
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for sig in HOST {
                libc::sigaddset(&mut set, sig);
                if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
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
