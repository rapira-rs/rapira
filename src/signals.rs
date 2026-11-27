use std::io;
use std::sync::{OnceLock, mpsc};
use std::thread;

const HOST: [libc::c_int; 6] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGHUP,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

/// The dedicated signal thread accepts the host signals through POSIX `sigwait`.
static SIGNAL_THREAD: OnceLock<libc::pthread_t> = OnceLock::new();

/// A PHP thread blocks no signal, so a host signal can arrive there. The handler passes the signal to the signal thread. `pthread_kill` is async-signal-safe. https://man7.org/linux/man-pages/man7/signal-safety.7.html
extern "C" fn forward(sig: libc::c_int) {
    if let Some(target) = SIGNAL_THREAD.get() {
        // SAFETY: the signal thread lives until the process exits.
        unsafe { libc::pthread_kill(*target, sig) };
    }
}

pub struct Signals(libc::sigset_t);

impl Signals {
    /// Blocks the host signals on the main thread before it creates any thread, and installs the handler for them. Each new thread inherits the mask. https://man7.org/linux/man-pages/man3/pthread_sigmask.3.html
    pub fn block() -> io::Result<Self> {
        // SAFETY: sigemptyset initializes both sets. pthread_sigmask and sigaction copy their arguments.
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for sig in HOST {
                libc::sigaddset(&mut set, sig);
            }
            // Block host signals before installing `forward` so startup signals stay pending until `sigwait` starts.
            let err = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            if err != 0 {
                return Err(io::Error::from_raw_os_error(err));
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward as *const () as libc::sighandler_t;
            // An interrupted system call of a PHP thread starts again.
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            for sig in HOST {
                if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(Self(set))
        }
    }

    /// Starts the dedicated `sigwait` consumer thread before any PHP worker spawns.
    pub fn start(self, on_signal: impl Fn(libc::c_int) + Send + 'static) -> io::Result<()> {
        let set = self.0;
        let (ready_tx, ready_rx) = mpsc::sync_channel::<()>(0);
        thread::Builder::new()
            .name("rapira-sig".into())
            .spawn(move || {
                // SAFETY: `pthread_self` returns the thread ID of this long-lived signal thread.
                let _ = SIGNAL_THREAD.set(unsafe { libc::pthread_self() });
                let _ = ready_tx.send(());
                loop {
                    let mut sig = 0;
                    // SAFETY: `set` contains only blocked host signals, and this thread is their sole `sigwait` consumer.
                    let err = unsafe { libc::sigwait(&set, &mut sig) };
                    match err {
                        0 => on_signal(sig),
                        libc::EINTR => continue,
                        err => {
                            tracing::error!(
                                target: "rapira",
                                "waiting for a host signal failed: {}",
                                io::Error::from_raw_os_error(err)
                            );
                            break;
                        }
                    }
                }
            })?;
        let _ = ready_rx.recv();
        Ok(())
    }
}
