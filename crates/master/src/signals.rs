//! The master signal path: the early block, the self-pipe handler and the sigwait helper of the workers.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::Context as _;
use libc::c_int;

/// Write end of the self-pipe; `-1` until install, set once before handlers are armed.
static SELF_PIPE_WR: AtomicI32 = AtomicI32::new(-1);

/// The full master disposition set: installed in the master, reset in children.
pub(crate) const MASTER_SIGNALS: [c_int; 7] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGQUIT,
    libc::SIGCHLD,
    libc::SIGHUP,
];

/// The signals a worker accepts by sigwait. The fork bracket leaves exactly these blocked in the child.
pub(crate) const WORKER_SIGNALS: [c_int; 3] = [libc::SIGQUIT, libc::SIGINT, libc::SIGUSR2];

pub(crate) struct SelfPipe {
    pub rd: UnixStream,
    pub wr: UnixStream,
}

impl Drop for SelfPipe {
    /// Disarm the handler before the fds close: a later signal must not write into a reused fd number.
    fn drop(&mut self) {
        SELF_PIPE_WR.store(-1, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
fn errno_location() -> *mut c_int {
    // SAFETY: libc provides the thread-local errno slot address.
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
fn errno_location() -> *mut c_int {
    // SAFETY: libc provides the thread-local errno slot address.
    unsafe { libc::__error() }
}

fn errno_get() -> c_int {
    // SAFETY: reading the thread-local errno slot.
    unsafe { *errno_location() }
}

fn errno_set(v: c_int) {
    // SAFETY: writing the thread-local errno slot.
    unsafe { *errno_location() = v }
}

/// Async-signal-safe (`write`, errno save/restore).
extern "C" fn master_sig_handler(signo: c_int) {
    // The byte is the signal number; every master signal is below 256 on Linux and macOS.
    let byte = signo as u8;
    let saved = errno_get();
    let fd = SELF_PIPE_WR.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write to a valid fd from a 1-byte stack buffer.
        unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
    }
    errno_set(saved);
}

pub(crate) fn sigset(sigs: &[c_int]) -> libc::sigset_t {
    // SAFETY: zeroed sigset_t is initialized in full by sigemptyset below.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: set points to a live sigset_t for the duration of these calls.
    unsafe {
        libc::sigemptyset(&mut set);
        for &s in sigs {
            libc::sigaddset(&mut set, s);
        }
    }
    set
}

/// Waits until one of the worker signals (blocked in every thread of the worker) is pending and accepts it, so no handler runs. https://man7.org/linux/man-pages/man3/sigwait.3.html
pub fn wait_worker_signal() -> c_int {
    let set = sigset(&WORKER_SIGNALS);
    let mut sig: c_int = 0;
    // SAFETY: set and sig are stack values live for the whole call.
    unsafe { libc::sigwait(&set, &mut sig) };
    sig
}

fn sigprocmask(how: c_int, set: &libc::sigset_t) {
    // SAFETY: set is a live sigset_t; null old mask discards the previous set.
    unsafe { libc::sigprocmask(how, set, std::ptr::null_mut()) };
}

/// Blocks USR1, USR2 and HUP until the master handlers exist: their default action ends the process. As pid 1 the same for TERM, INT and QUIT: the kernel drops a signal to an init process whose action is SIG_DFL, and a blocked signal stays pending instead. https://man7.org/linux/man-pages/man2/kill.2.html#NOTES
/// USR1 also gets SIG_IGN: PHP MINIT saves the current action of every signal, and a worker's Zend handler forwards USR1 to that saved action from its second request on. https://github.com/php/php-src/blob/PHP-8.5/Zend/zend_signal.c
pub fn block_early_signals() {
    sigprocmask(
        libc::SIG_BLOCK,
        &sigset(&[libc::SIGUSR1, libc::SIGUSR2, libc::SIGHUP]),
    );
    // signal is portable for SIG_IGN and SIG_DFL. https://man7.org/linux/man-pages/man2/signal.2.html#VERSIONS
    // SAFETY: SIG_IGN installs no handler code.
    unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
    // SAFETY: getpid is always safe.
    if unsafe { libc::getpid() } == 1 {
        sigprocmask(
            libc::SIG_BLOCK,
            &sigset(&[libc::SIGTERM, libc::SIGINT, libc::SIGQUIT]),
        );
    }
}

/// Must run after PHP MINIT: it saves the action of every signal, and a worker's Zend handler forwards TERM, HUP and USR1 to that saved action from its second request on. Installed earlier, the master handler would be in that table and run inside a worker.
pub(crate) fn install_master_signals() -> anyhow::Result<SelfPipe> {
    let (rd, wr) = UnixStream::pair().context("self-pipe socketpair")?;
    rd.set_nonblocking(true)?;
    wr.set_nonblocking(true)?;
    SELF_PIPE_WR.store(wr.as_raw_fd(), Ordering::Relaxed);

    // SAFETY: act is fully initialized, mask is a live sigset_t, null old-action pointer discards the previous handler.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = master_sig_handler as *const () as usize;
        libc::sigfillset(&mut act.sa_mask);
        for sig in MASTER_SIGNALS {
            libc::sigaction(sig, &act, std::ptr::null_mut());
        }
    }
    sigprocmask(libc::SIG_SETMASK, &sigset(&[]));

    Ok(SelfPipe { rd, wr })
}
