//! The master loop: a poll on the self-pipe, the signal routing, the reap and the deadlines.

use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::time::{Duration, Instant};

use libc::c_int;

use crate::pctl::{Pctl, SignalAction};
use crate::pool::Pool;
use crate::process::{Forker, reap_all};
use crate::{MasterConfig, StopReason, WORKER_EXIT_DRAINED};

/// Milliseconds until `next`, rounded up so a sub-millisecond remainder never busy-spins `poll` with a 0 timeout. At most 1000: `next` is never later than the 1 s tick.
fn poll_timeout_ms(next: Instant, now: Instant) -> c_int {
    let d = next.saturating_duration_since(now);
    d.as_nanos().div_ceil(1_000_000) as c_int
}

/// The master-wide part: signals, the stop escalation and the routing to the pools. Every per-pool decision lives in [`Pool`].
pub(crate) struct Master {
    pools: Vec<Pool>,
    spawner: Forker,
    pctl: Pctl,
    control_timeout: Duration,
    next_tick: Instant,
}

impl Master {
    /// Gives each pool the slots of its region.
    pub(crate) fn new(cfg: MasterConfig, spawner: Forker) -> Master {
        let now = Instant::now();
        let mut pools: Vec<Pool> = Vec::with_capacity(cfg.pools.len());
        for (i, p) in cfg.pools.into_iter().enumerate() {
            let slots = spawner.regions[i].slots.clone();
            pools.push(Pool::new(i, p, &spawner.board[slots]));
        }
        Master {
            pools,
            spawner,
            pctl: Pctl::default(),
            control_timeout: cfg.process_control_timeout,
            next_tick: now + Duration::from_secs(1),
        }
    }

    /// `Some(reason)` means the loop must return now: forced stop, or a stop with nothing left to drain.
    fn handle_signal(&mut self, signo: c_int, now: Instant) -> Option<StopReason> {
        match self.pctl.on_signal(signo, now + self.control_timeout) {
            SignalAction::Stop => self.begin_stop(),
            SignalAction::Forced => {
                for p in &self.pools {
                    p.signal_all(libc::SIGTERM);
                }
                Some(StopReason::Forced)
            }
            SignalAction::Reload => {
                self.spawner.reloads.add_sent();
                // Each worker restarts its app on its own.
                for p in &self.pools {
                    p.signal_all(libc::SIGUSR2);
                }
                None
            }
            SignalAction::Status => {
                for p in &self.pools {
                    p.log_status();
                }
                None
            }
            SignalAction::Ignore => None,
        }
    }

    fn begin_stop(&mut self) -> Option<StopReason> {
        for p in &mut self.pools {
            p.begin_stop();
        }
        // No worker starts after this point. The old closure drops and closes the listeners of the master, so new clients get a refused connection.
        self.spawner.worker = Box::new(|_| WORKER_EXIT_DRAINED);
        // With no live worker no SIGCHLD comes, so only this check ends the stop.
        if self.drained() {
            return Some(StopReason::Drained);
        }
        None
    }

    fn drained(&self) -> bool {
        self.pools.iter().all(|p| p.procs.is_empty())
    }

    fn fire_due_deadlines(&mut self, now: Instant) {
        if self.pctl.stop_deadline().is_some_and(|t| now >= t) {
            let sig = self.pctl.escalate(now);
            for p in &self.pools {
                p.signal_all(sig);
            }
        }
        for p in &mut self.pools {
            p.fire_due(now, &mut self.spawner);
        }
        if now >= self.next_tick {
            self.next_tick = now + Duration::from_secs(1);
            // Nothing runs while the master stops: the stop escalation bounds every worker.
            if !self.pctl.is_stopping() {
                for p in &mut self.pools {
                    p.maintenance_tick();
                }
            }
        }
    }

    fn next_deadline(&self) -> Instant {
        let mut next = self.next_tick;
        for p in &self.pools {
            if let Some(t) = p.next_deadline() {
                next = next.min(t);
            }
        }
        if let Some(t) = self.pctl.stop_deadline() {
            next = next.min(t);
        }
        next
    }

    pub(crate) fn run_loop(&mut self) -> anyhow::Result<StopReason> {
        let start = Instant::now();
        for p in &mut self.pools {
            p.spawn_all(start, &mut self.spawner);
        }
        loop {
            let mut pfd = libc::pollfd {
                fd: self.spawner.self_pipe.rd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };

            let timeout = poll_timeout_ms(self.next_deadline(), Instant::now());
            // SAFETY: pfd is a live pollfd; timeout is a valid millisecond count.
            let n = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                anyhow::bail!("poll: {e}");
            }

            let now = Instant::now();
            let mut got_chld: bool = false;
            if n > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let mut buf = [0u8; 64];
                // Drain the nonblocking self-pipe: WouldBlock (empty) or an error ends the loop.
                while let Ok(len @ 1..) = (&self.spawner.self_pipe.rd).read(&mut buf) {
                    for &b in &buf[..len] {
                        if b == libc::SIGCHLD as u8 {
                            got_chld = true;
                        } else if let Some(reason) = self.handle_signal(c_int::from(b), now) {
                            return Ok(reason);
                        }
                    }
                }
            }

            if got_chld {
                reap_all(&mut self.pools, now, self.pctl.is_stopping())?;
                if self.pctl.is_stopping() && self.drained() {
                    return Ok(StopReason::Drained);
                }
            }

            self.fire_due_deadlines(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_timeout_rounds_up_never_to_zero_before_deadline() {
        struct Case {
            name: &'static str,
            next_after_now: Duration,
            now_after_next: Duration,
            want: c_int,
        }
        let cases = [
            Case {
                name: "1.5 ms rounds up to 2",
                next_after_now: Duration::from_micros(1500),
                now_after_next: Duration::ZERO,
                want: 2,
            },
            Case {
                name: "1 ns rounds up to 1",
                next_after_now: Duration::from_nanos(1),
                now_after_next: Duration::ZERO,
                want: 1,
            },
            Case {
                name: "a whole 3 ms stays 3",
                next_after_now: Duration::from_millis(3),
                now_after_next: Duration::ZERO,
                want: 3,
            },
            Case {
                name: "a past deadline gives 0",
                next_after_now: Duration::ZERO,
                now_after_next: Duration::from_millis(5),
                want: 0,
            },
        ];
        let base = Instant::now();
        for case in cases {
            assert_eq!(
                poll_timeout_ms(base + case.next_after_now, base + case.now_after_next),
                case.want,
                "{}",
                case.name
            );
        }
    }
}
