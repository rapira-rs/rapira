use std::io::Read as _;
use std::os::fd::OwnedFd;

/// On lifeline EOF the SIGQUIT must be process-directed (`kill`, not `raise`): a thread-directed blocked signal stays in this thread's pending set where the worker's `sigwait` thread never sees it.
pub fn spawn_lifeline_watch(lifeline: OwnedFd) {
    std::thread::Builder::new()
        .name("rapira-lifeline".into())
        .spawn(move || {
            let mut rd = std::io::PipeReader::from(lifeline);
            if let Err(e) = rd.read_exact(&mut [0u8])
                && e.kind() == std::io::ErrorKind::UnexpectedEof
            {
                tracing::warn!(target: "rapira", "master died (lifeline EOF); draining");
                // SAFETY: kill takes no pointers.
                unsafe { libc::kill(libc::getpid(), libc::SIGQUIT) };
            }
        })
        .expect("spawn lifeline thread");
}
