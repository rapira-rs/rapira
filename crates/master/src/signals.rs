use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::Context as _;
use libc::c_int;

/// Write end of the self-pipe; `-1` until install, set once before handlers are armed.
static SELF_PIPE_WR: AtomicI32 = AtomicI32::new(-1);

/// Control bytes emitted by the handler, consumed by the poll loop.
pub(crate) const SIG_TERM: u8 = b'T';
pub(crate) const SIG_INT: u8 = b'I';
pub(crate) const SIG_USR1: u8 = b'1';
pub(crate) const SIG_USR2: u8 = b'2';
pub(crate) const SIG_QUIT: u8 = b'Q';
pub(crate) const SIG_CHLD: u8 = b'C';
pub(crate) const SIG_HUP: u8 = b'H';

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

pub(crate) struct SelfPipe {
    pub rd: OwnedFd,
    pub wr: OwnedFd,
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

pub(crate) fn errno_get() -> c_int {
    // SAFETY: reading the thread-local errno slot.
    unsafe { *errno_location() }
}

fn errno_set(v: c_int) {
    // SAFETY: writing the thread-local errno slot we just read.
    unsafe { *errno_location() = v }
}

/// Async-signal-safe (`write`, errno save/restore).
extern "C" fn master_sig_handler(signo: c_int) {
    let byte: u8 = match signo {
        libc::SIGTERM => SIG_TERM,
        libc::SIGINT => SIG_INT,
        libc::SIGUSR1 => SIG_USR1,
        libc::SIGUSR2 => SIG_USR2,
        libc::SIGQUIT => SIG_QUIT,
        libc::SIGCHLD => SIG_CHLD,
        libc::SIGHUP => SIG_HUP,
        _ => return,
    };
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

/// Blocks until one of `signals` (already blocked) is delivered. https://man7.org/linux/man-pages/man3/sigwait.3.html
pub fn wait_signal(signals: &[c_int]) -> c_int {
    let set = sigset(signals);
    let mut sig: c_int = 0;
    // SAFETY: `set` and `sig` are stack values live for the whole call.
    unsafe { libc::sigwait(&set, &mut sig) };
    sig
}

fn sigprocmask(how: c_int, set: &libc::sigset_t) {
    // SAFETY: set is a live sigset_t; null old mask discards the previous set.
    unsafe { libc::sigprocmask(how, set, std::ptr::null_mut()) };
}

/// Blocks the terminate-by-default signals before any handler exists; as pid 1 the stop trio joins them because a SIGNAL_UNKILLABLE init drops signals still on SIG_DFL instead of applying the default action. https://man7.org/linux/man-pages/man7/signal.7.html
pub fn block_early_signals() {
    sigprocmask(
        libc::SIG_BLOCK,
        &sigset(&[libc::SIGUSR1, libc::SIGUSR2, libc::SIGCHLD, libc::SIGHUP]),
    );
    // SAFETY: getpid is always safe.
    if unsafe { libc::getpid() } == 1 {
        sigprocmask(
            libc::SIG_BLOCK,
            &sigset(&[libc::SIGTERM, libc::SIGINT, libc::SIGQUIT]),
        );
    }
}

/// Must run in the master after PHP MINIT; Zend leaves these dispositions alone because the master never calls `php_request_startup`.
pub(crate) fn install_master_signals() -> anyhow::Result<SelfPipe> {
    // Both ends are CLOEXEC.
    let (rd, wr) = UnixStream::pair().context("self-pipe socketpair")?;
    rd.set_nonblocking(true)?;
    wr.set_nonblocking(true)?;
    SELF_PIPE_WR.store(wr.as_raw_fd(), Ordering::Relaxed);

    // SAFETY: act is fully initialized, mask is a live sigset_t, null old-action pointer discards the previous handler.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = master_sig_handler as *const () as usize;
        libc::sigfillset(&mut act.sa_mask);
        act.sa_flags = 0;
        for sig in MASTER_SIGNALS {
            anyhow::ensure!(
                libc::sigaction(sig, &act, std::ptr::null_mut()) == 0,
                "sigaction({sig}): {}",
                io::Error::last_os_error()
            );
        }
    }

    let mut all: libc::sigset_t = sigset(&[]);
    // SAFETY: all is a live sigset_t.
    unsafe { libc::sigfillset(&mut all) };
    sigprocmask(libc::SIG_UNBLOCK, &all);

    Ok(SelfPipe {
        rd: rd.into(),
        wr: wr.into(),
    })
}
