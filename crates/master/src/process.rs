//! Worker processes: the fork bracket, the reap and its verdicts, the respawn backoff and the RSS read.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use libc::c_int;

use crate::WorkerEnv;
use crate::pool::Pool;
use crate::signals::{MASTER_SIGNALS, SelfPipe, WORKER_SIGNALS, sigset};
use crate::{WORKER_EXIT_DRAINED, WORKER_EXIT_UNHEALTHY};
use rapira_scoreboard::{PoolRegion, SharedSlot};

/// A worker that lived at least this long before it failed starts a new backoff streak.
const QUICK_CRASH: Duration = Duration::from_secs(10);
/// Respawn delay after the first failure of a streak. Each further failure doubles it, up to 2^8 times this value (25.6 s).
const RESPAWN_BASE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkerProc {
    pub pid: libc::pid_t,
    pub slot: usize,
    pub spawned_at: Instant,
    /// The request watchdog sent SIGTERM: a SIGTERM or SIGKILL exit is a timeout kill, not a crash.
    pub timeout_kill: bool,
    /// The memory limit sent SIGQUIT: the worker drains, so the limit does not signal it again.
    pub memory_drain: bool,
}

/// Respawn state of one slot.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SlotState {
    /// Quick failures in a row; it selects the backoff delay.
    pub crash_streak: u32,
    /// When the next fork is due; None while nothing is scheduled.
    pub respawn_at: Option<Instant>,
}

impl SlotState {
    /// Schedules a respawn after a crash, an unhealthy exit or a failed fork. `lived` is how long the dead worker ran, or zero after a failed fork; a life of `QUICK_CRASH` or more resets the streak.
    pub fn schedule_backoff(&mut self, lived: Duration, now: Instant) {
        if lived >= QUICK_CRASH {
            self.crash_streak = 0;
        }
        let delay = backoff_delay(self.crash_streak);
        self.crash_streak += 1;
        self.respawn_at = Some(now + delay);
    }

    /// Schedules a respawn now and ends the streak: the worker drained or the watchdog ended it.
    pub fn schedule_immediate(&mut self, now: Instant) {
        self.crash_streak = 0;
        self.respawn_at = Some(now);
    }
}

fn backoff_delay(streak: u32) -> Duration {
    RESPAWN_BASE * (1u32 << streak.min(8))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitVerdict {
    Drain,
    Unhealthy,
    TimeoutKill,
    Crash,
}

/// `waitpid` without WUNTRACED reports an exit or a signal, nothing else.
fn classify(status: c_int, timeout_kill: bool) -> ExitVerdict {
    if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            WORKER_EXIT_DRAINED => ExitVerdict::Drain,
            WORKER_EXIT_UNHEALTHY => ExitVerdict::Unhealthy,
            _ => ExitVerdict::Crash,
        }
    } else {
        let sig = libc::WTERMSIG(status);
        if timeout_kill && (sig == libc::SIGTERM || sig == libc::SIGKILL) {
            ExitVerdict::TimeoutKill
        } else {
            ExitVerdict::Crash
        }
    }
}

/// A dead worker removed from its pool.
pub(crate) struct Buried {
    /// Index of the pool that owned the worker.
    pub pool: usize,
    pub worker: WorkerProc,
    pub verdict: ExitVerdict,
}

/// Routes one dead pid to the pool that owns it. `waitpid(-1)` returns a bare pid, so the pools resolve the owner.
pub(crate) fn bury(pools: &mut [Pool], pid: libc::pid_t, status: c_int) -> Option<Buried> {
    for (i, pool) in pools.iter_mut().enumerate() {
        let Some(at) = pool.procs.iter().position(|p| p.pid == pid) else {
            continue;
        };
        let w = pool.procs.swap_remove(at);
        return Some(Buried {
            pool: i,
            worker: w,
            verdict: classify(status, w.timeout_kill),
        });
    }
    None
}

/// Drains `waitpid` fully so every child ready at this point is buried in one pass. Each crash outside a stop gets an error record; a stop ends workers with signals and forced exits on purpose.
pub(crate) fn reap_all(pools: &mut [Pool], now: Instant, stopping: bool) -> anyhow::Result<()> {
    loop {
        let mut status: c_int = 0;
        // SAFETY: standard non-blocking reap; status is a live out-param.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return Ok(());
        }
        let Some(Buried {
            pool,
            worker,
            verdict,
        }) = bury(pools, pid, status)
        else {
            // As pid 1 the master adopts orphans that PHP code left behind; waitpid(-1) reaps them here.
            tracing::warn!(target: "master", "reaped unknown child {pid}");
            continue;
        };
        if verdict == ExitVerdict::Crash && !stopping {
            let how = if libc::WIFSIGNALED(status) {
                format!("killed by signal {}", libc::WTERMSIG(status))
            } else {
                format!("exited with code {}", libc::WEXITSTATUS(status))
            };
            tracing::error!(
                target: "master",
                "{} pool: worker {pid} {how}",
                pools[pool].cfg.name
            );
        }
        pools[pool].on_child_exit(worker, verdict, now, stopping)?;
    }
}

/// The resident set of `pid` in bytes: the second field of `/proc/<pid>/statm`, in pages. https://man7.org/linux/man-pages/man5/proc_pid_statm.5.html
#[cfg(target_os = "linux")]
pub(crate) fn rss(pid: libc::pid_t) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf takes no pointers.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages * page as u64)
}
/// The resident set of `pid` in bytes, from `proc_pidinfo(PROC_PIDTASKINFO)`.
#[cfg(target_os = "macos")]
pub(crate) fn rss(pid: libc::pid_t) -> Option<u64> {
    // SAFETY: proc_taskinfo is plain integers, so all zeroes is a valid value.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as c_int;
    // SAFETY: info is a live proc_taskinfo of size bytes.
    let n =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
    (n == size).then_some(info.pti_resident_size)
}

/// The result is not checked: a pid that is already gone is reaped on the next SIGCHLD.
pub(crate) fn kill(pid: libc::pid_t, sig: c_int) {
    // SAFETY: kill takes no pointers.
    unsafe { libc::kill(pid, sig) };
}

/// Fork source for every pool. The boxed worker closure keeps `Pool` and `Master` free of the closure's type.
pub(crate) struct Forker {
    pub self_pipe: SelfPipe,
    /// Read end; `spawn` clones it for the lifeline thread of each child.
    pub lifeline_rd: std::io::PipeReader,
    /// Never written: its close at master exit is what signals master death to every worker's `lifeline_rd`.
    pub lifeline_wr: std::io::PipeWriter,
    /// The whole board, for every `WorkerEnv`.
    pub board: &'static [SharedSlot],
    /// The slot range of each pool, in pool order, for every `WorkerEnv`.
    pub regions: &'static [PoolRegion],
    pub worker: Box<dyn FnMut(WorkerEnv) -> i32>,
}

impl Forker {
    /// Forks one worker of pool `pool` and returns its pid. The child runs the worker closure and `_exit`s with its code, so no master Drop (pidfile unlink, PHP shutdown) runs in a child.
    pub(crate) fn spawn(
        &mut self,
        pool: usize,
        slot_view: &'static SharedSlot,
    ) -> std::io::Result<libc::pid_t> {
        // A copy of the read end for the lifeline thread of the child: EOF on it means the master died. The parent closes its copy when this function returns.
        let lifeline_rd = self.lifeline_rd.try_clone()?;

        // The child inherits this mask, so the master signals stay blocked until the child resets their handlers. Otherwise a signal in that window runs the master handler in the child, which writes a second byte into the shared self-pipe: a Ctrl-C would reach the master twice and force the stop.
        let block: libc::sigset_t = sigset(&MASTER_SIGNALS);
        // SAFETY: zeroed sigset_t is fully overwritten by sigprocmask's out-param.
        let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: block/old are live sigset_ts.
        unsafe { libc::sigprocmask(libc::SIG_BLOCK, &block, &mut old) };

        // SAFETY: fork in a single-threaded master, so no other thread holds a lock the child inherits.
        match unsafe { libc::fork() } {
            // Child: it becomes the worker and never returns from this arm.
            0 => {
                // SAFETY: all calls below are async-signal-safe or operate on fds we own.
                unsafe {
                    // Only the master uses the self-pipe.
                    libc::close(self.self_pipe.rd.as_raw_fd());
                    libc::close(self.self_pipe.wr.as_raw_fd());
                    // The master keeps the only write end, so its exit gives EOF on the lifeline of every worker.
                    libc::close(self.lifeline_wr.as_raw_fd());

                    // The master handler writes into the self-pipe, which is closed now. Every master signal goes back to its default action.
                    for s in MASTER_SIGNALS {
                        libc::signal(s, libc::SIG_DFL);
                    }
                    // USR1 asks the master for a status log and means nothing to a worker. Its default action ends the process, so the worker ignores it.
                    libc::signal(libc::SIGUSR1, libc::SIG_IGN);

                    // The process name that ps and top (and btop) show. https://man7.org/linux/man-pages/man2/PR_SET_NAME.2const.html
                    #[cfg(target_os = "linux")]
                    libc::prctl(libc::PR_SET_NAME, c"rapira-worker".as_ptr());

                    // This mask replaces the inherited one. Only the worker signals stay blocked, for the sigwait thread of the worker (wait_worker_signal). Every other signal is unblocked.
                    // USR2 keeps SIG_DFL: POSIX lets a kernel discard an ignored signal even while it is blocked. https://pubs.opengroup.org/onlinepubs/9799919799/functions/V2_chap02.html#tag_16_04_01
                    let hold = sigset(&WORKER_SIGNALS);
                    libc::sigprocmask(libc::SIG_SETMASK, &hold, std::ptr::null_mut());
                }

                // The child still has the stack frames of the master from the fork. A panic stops here: unwinding into those frames would run master Drops in the child.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (self.worker)(WorkerEnv {
                        pool,
                        lifeline: lifeline_rd,
                        slot_view,
                        board: self.board,
                        regions: self.regions,
                    })
                }));
                // A panic exits with 101, which the master counts as a crash.
                let code = match outcome {
                    Ok(code) => code,
                    Err(_) => {
                        tracing::error!(target: "master", "worker child panicked; exiting");
                        101
                    }
                };
                // The child copied the atexit handlers and the stdio buffers of the master. _exit runs none of the handlers and flushes no buffer. https://man7.org/linux/man-pages/man2/_exit.2.html
                // SAFETY: async-signal-safe process exit in the child.
                unsafe { libc::_exit(code) }
            }
            // The fork failed, so no child exists. The mask goes back before the error returns.
            -1 => {
                // SAFETY: restore the pre-fork mask.
                unsafe { libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
                Err(std::io::Error::last_os_error())
            }
            // Parent: pid is the new worker. A master signal that came during the fork is handled after the mask goes back.
            pid => {
                // SAFETY: restore the pre-fork mask.
                unsafe { libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
                Ok(pid)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Raw wait-status encoding (BSD-derived, shared by Linux and macOS): exit code in bits 8..16, signal in the low 7 bits.
    fn exited(code: i32) -> c_int {
        code << 8
    }
    fn signaled(sig: c_int) -> c_int {
        sig
    }

    #[test]
    fn classify_maps_a_wait_status_to_its_verdict() {
        struct Case {
            name: &'static str,
            status: c_int,
            timeout_kill: bool,
            want: ExitVerdict,
        }
        let cases = [
            Case {
                name: "exit 0 drained",
                status: exited(0),
                timeout_kill: false,
                want: ExitVerdict::Drain,
            },
            Case {
                name: "exit 0 after a timeout TERM still drained",
                status: exited(0),
                timeout_kill: true,
                want: ExitVerdict::Drain,
            },
            Case {
                name: "exit 89 unhealthy",
                status: exited(89),
                timeout_kill: false,
                want: ExitVerdict::Unhealthy,
            },
            Case {
                name: "any other exit code crashed",
                status: exited(42),
                timeout_kill: false,
                want: ExitVerdict::Crash,
            },
            Case {
                name: "TERM after the watchdog is a timeout kill",
                status: signaled(libc::SIGTERM),
                timeout_kill: true,
                want: ExitVerdict::TimeoutKill,
            },
            Case {
                name: "KILL after the watchdog is a timeout kill",
                status: signaled(libc::SIGKILL),
                timeout_kill: true,
                want: ExitVerdict::TimeoutKill,
            },
            Case {
                name: "another signal after the watchdog crashed",
                status: signaled(libc::SIGSEGV),
                timeout_kill: true,
                want: ExitVerdict::Crash,
            },
            Case {
                name: "TERM without the watchdog crashed",
                status: signaled(libc::SIGTERM),
                timeout_kill: false,
                want: ExitVerdict::Crash,
            },
            Case {
                name: "KILL without the watchdog crashed",
                status: signaled(libc::SIGKILL),
                timeout_kill: false,
                want: ExitVerdict::Crash,
            },
            Case {
                name: "SEGV crashed",
                status: signaled(libc::SIGSEGV),
                timeout_kill: false,
                want: ExitVerdict::Crash,
            },
        ];
        for case in cases {
            assert_eq!(
                classify(case.status, case.timeout_kill),
                case.want,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn backoff_doubles_per_crash_up_to_the_cap() {
        struct Case {
            name: &'static str,
            streak: u32,
            want: Duration,
        }
        let cases = [
            Case {
                name: "first crash",
                streak: 0,
                want: Duration::from_millis(100),
            },
            Case {
                name: "second crash",
                streak: 1,
                want: Duration::from_millis(200),
            },
            Case {
                name: "third crash",
                streak: 2,
                want: Duration::from_millis(400),
            },
            Case {
                name: "the cap at 2^8",
                streak: 8,
                want: Duration::from_millis(25_600),
            },
            Case {
                name: "one past the cap",
                streak: 9,
                want: Duration::from_millis(25_600),
            },
            Case {
                name: "far past the cap",
                streak: 100,
                want: Duration::from_millis(25_600),
            },
        ];
        for case in cases {
            assert_eq!(backoff_delay(case.streak), case.want, "{}", case.name);
        }
    }

    #[test]
    fn schedule_backoff_increments_streak() {
        let now = Instant::now();
        let mut s = SlotState::default();
        s.schedule_backoff(Duration::from_millis(1), now);
        assert_eq!(s.crash_streak, 1);
        assert_eq!(s.respawn_at, Some(now + Duration::from_millis(100)));
        s.schedule_backoff(Duration::from_millis(1), now);
        assert_eq!(s.crash_streak, 2);
        assert_eq!(s.respawn_at, Some(now + Duration::from_millis(200)));
    }

    #[test]
    fn long_lived_crash_resets_streak() {
        let now = Instant::now();
        let mut s = SlotState {
            crash_streak: 5,
            respawn_at: None,
        };
        s.schedule_backoff(QUICK_CRASH, now);
        assert_eq!(s.crash_streak, 1);
        assert_eq!(s.respawn_at, Some(now + Duration::from_millis(100)));
    }
}
