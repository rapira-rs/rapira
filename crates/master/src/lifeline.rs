use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::Context as _;

/// `wr` is never written: its disappearance is what signals master death to every worker's `rd`.
pub(crate) struct Lifeline {
    pub(crate) rd: OwnedFd,
    pub(crate) wr: OwnedFd,
}

impl Lifeline {
    /// Both ends are `CLOEXEC`: fork still inherits them, the flag only keeps them out of exec'd processes.
    pub(crate) fn create() -> anyhow::Result<Lifeline> {
        let (rd, wr) = std::io::pipe().context("lifeline pipe")?;
        Ok(Lifeline {
            rd: rd.into(),
            wr: wr.into(),
        })
    }
}

/// On lifeline EOF the SIGQUIT must be process-directed (`kill`, not `raise`): a thread-directed blocked signal stays in this thread's pending set where the worker's `sigwait` thread never sees it.
pub fn spawn_lifeline_watch(lifeline: OwnedFd) {
    std::thread::Builder::new()
        .name("rapira-lifeline".into())
        .spawn(move || {
            let mut byte = 0u8;
            loop {
                // SAFETY: reads into a 1-byte stack buffer on an fd we own.
                let n = unsafe { libc::read(lifeline.as_raw_fd(), (&raw mut byte).cast(), 1) };
                if n == 0 {
                    tracing::warn!(target: "rapira", "master died (lifeline EOF); draining");
                    unsafe { libc::kill(libc::getpid(), libc::SIGQUIT) };
                    return;
                }
                if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return;
            }
        })
        .expect("spawn lifeline thread");
}
