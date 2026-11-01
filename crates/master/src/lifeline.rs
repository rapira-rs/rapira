//! The worker side of the lifeline pipe: a thread that drains the worker when the master dies.

use std::io::Read as _;

/// Starts a thread that sends SIGQUIT to this process when `lifeline` reaches EOF (the master died), so the worker drains. The signal must be process-directed: in a threaded program `raise` is `pthread_kill(pthread_self(), sig)`, and a signal pending for this thread never reaches the sigwait thread of the worker. https://man7.org/linux/man-pages/man3/raise.3.html#DESCRIPTION
pub fn spawn_lifeline_watch(mut lifeline: std::io::PipeReader) {
    std::thread::Builder::new()
        .name("rapira-lifeline".into())
        .spawn(move || {
            // Nothing writes the pipe, so the read ends only at EOF.
            let _ = lifeline.read_exact(&mut [0u8]);
            tracing::warn!(target: "rapira", "master died (lifeline EOF); draining");
            // SAFETY: kill takes no pointers.
            unsafe { libc::kill(libc::getpid(), libc::SIGQUIT) };
        })
        .expect("spawn lifeline thread");
}
