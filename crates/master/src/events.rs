use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use libc::c_int;
use rapira_scoreboard::Scoreboard;

use crate::pctl::{Pctl, SignalAction};
use crate::pool::Pool;
use crate::process::{ExitVerdict, Forker, ProcTable, WorkerProc, reap_all};
use crate::signals::{SIG_CHLD, errno_get};
use crate::{MasterConfig, StopReason};

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Milliseconds until `next`, rounded up so a sub-millisecond remainder never busy-spins `poll` with a 0 timeout.
fn poll_timeout_ms(next: Instant, now: Instant) -> c_int {
    let d = next.saturating_duration_since(now);
    let ms = d.as_nanos().div_ceil(1_000_000);
    ms.min(i32::MAX as u128) as c_int
}

fn drain_pipe(fd: RawFd, buf: &mut [u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        // SAFETY: read into a live buffer from a valid nonblocking fd.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

/// The global half: signals, the stop escalation, and routing to the pools. Every per-pool decision lives in [`Pool`].
pub(crate) struct Master<'w> {
    pools: Vec<Pool>,
    spawner: Forker<'w>,
    pctl: Pctl,
    control_timeout: Duration,
    next_tick: Instant,
}

impl<'w> Master<'w> {
    /// Lays the pools out over the board: two slots per worker, contiguous, in `cfg.pools` order.
    pub(crate) fn new(
        cfg: MasterConfig,
        scoreboard: Scoreboard,
        spawner: Forker<'w>,
    ) -> Master<'w> {
        let now = Instant::now();
        let control_timeout = cfg.process_control_timeout;
        let mut base: usize = 0;
        let pools: Vec<Pool> = cfg
            .pools
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let len = p.slots();
                let board = scoreboard.slice(base..base + len);
                base += len;
                Pool::new(i, p, board, control_timeout)
            })
            .collect();
        Master {
            pools,
            spawner,
            pctl: Pctl::default(),
            control_timeout,
            next_tick: now + Duration::from_secs(1),
        }
    }

    /// `Some(reason)` means the loop must return now: forced stop, or a stop with nothing left to drain.
    fn handle_signal(&mut self, byte: u8, now: Instant) -> Option<StopReason> {
        match self.pctl.on_signal(byte, now + self.control_timeout) {
            SignalAction::Stop => self.begin_stop(),
            SignalAction::Forced => {
                self.force_stop();
                Some(StopReason::Forced)
            }
            SignalAction::Reload => {
                // A pool that still drains its chain swallows the signal.
                if self.pools.iter().all(|p| p.reload.is_none()) {
                    self.begin_reload(now);
                }
                None
            }
            SignalAction::Status => {
                self.log_status();
                None
            }
            SignalAction::Ignore => None,
        }
    }

    fn begin_stop(&mut self) -> Option<StopReason> {
        for p in &mut self.pools {
            p.begin_stop();
        }
        if self.drained() {
            return Some(StopReason::Drained);
        }
        None
    }

    fn force_stop(&self) {
        for p in &self.pools {
            p.signal_all(libc::SIGTERM);
        }
    }

    fn escalate_stop(&mut self, now: Instant) {
        let sig = self.pctl.escalate(now);
        for p in &self.pools {
            p.signal_all(sig);
        }
    }

    fn drained(&self) -> bool {
        self.pools.iter().all(|p| p.table.procs.is_empty())
    }

    fn begin_reload(&mut self, now: Instant) {
        for p in &mut self.pools {
            p.begin_reload(now, &mut self.spawner);
        }
    }

    fn route_exit(
        &mut self,
        pool: usize,
        w: WorkerProc,
        verdict: ExitVerdict,
        now: Instant,
    ) -> anyhow::Result<()> {
        let stopping = self.pctl.is_stopping();
        self.pools[pool].on_child_exit(w, verdict, now, stopping, &mut self.spawner)?;
        Ok(())
    }

    fn reap(&mut self, now: Instant) -> anyhow::Result<()> {
        let buried = {
            let mut tables: Vec<&mut ProcTable> =
                self.pools.iter_mut().map(|p| &mut p.table).collect();
            reap_all(&mut tables)
        };
        for (pool, w, verdict) in buried {
            self.route_exit(pool, w, verdict, now)?;
        }
        Ok(())
    }

    fn fire_due_deadlines(&mut self, now: Instant) {
        if self.pctl.stop_deadline().is_some_and(|t| now >= t) {
            self.escalate_stop(now);
        }
        for p in &mut self.pools {
            p.fire_due(now, &mut self.spawner);
        }
        if now >= self.next_tick {
            self.next_tick = now + Duration::from_secs(1);
            let stopping = self.pctl.is_stopping();
            for p in &mut self.pools {
                p.maintenance_tick(now, stopping, &mut self.spawner);
            }
        }
    }

    fn next_deadline(&self) -> Instant {
        let mut next = self.next_tick;
        for d in self
            .pools
            .iter()
            .filter_map(|p| p.next_deadline())
            .chain(self.pctl.stop_deadline())
        {
            next = next.min(d);
        }
        next
    }

    /// `fds[0]` is the signal fd; `owners[k]` is the pool that owns `fds[k + 1]`.
    fn poll_set(&self) -> (Vec<libc::pollfd>, Vec<usize>) {
        let stopping = self.pctl.is_stopping();
        let mut fds: Vec<libc::pollfd> = vec![pollfd(self.spawner.signal_fd())];
        let mut owners: Vec<usize> = Vec::new();
        for (i, p) in self.pools.iter().enumerate() {
            if !p.armed(stopping) {
                continue;
            }
            for &fd in &p.cfg.listeners {
                fds.push(pollfd(fd));
                owners.push(i);
            }
        }
        (fds, owners)
    }

    /// Re-checks arming: this iteration's signals and reaps can disarm a pool after `poll` returned its listener readable, and a fork disarms its own pool.
    fn fork_readable(&mut self, readable: &[usize], now: Instant) {
        let stopping = self.pctl.is_stopping();
        for &i in readable {
            if self.pools[i].armed(stopping) {
                self.pools[i].ondemand_fork_one(now, &mut self.spawner);
            }
        }
    }

    fn log_status(&self) {
        for p in &self.pools {
            p.log_status();
        }
    }

    pub(crate) fn run_loop(&mut self) -> anyhow::Result<StopReason> {
        let start = Instant::now();
        for p in &mut self.pools {
            p.fork_initial(start, &mut self.spawner);
        }
        loop {
            let (mut fds, owners) = self.poll_set();

            let timeout = poll_timeout_ms(self.next_deadline(), Instant::now());
            // SAFETY: fds is a live slice; timeout is a valid millisecond count.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
            if n < 0 {
                if errno_get() == libc::EINTR {
                    continue;
                }
                anyhow::bail!("poll: {}", std::io::Error::last_os_error());
            }
            let now = Instant::now();

            let mut got_chld: bool = false;
            if n > 0 && (fds[0].revents & libc::POLLIN) != 0 {
                let mut buf = [0u8; 64];
                for b in drain_pipe(self.spawner.signal_fd(), &mut buf) {
                    if b == SIG_CHLD {
                        got_chld = true;
                    } else if let Some(reason) = self.handle_signal(b, now) {
                        return Ok(reason);
                    }
                }
            }

            if got_chld {
                self.reap(now)?;
                if self.pctl.is_stopping() && self.drained() {
                    return Ok(StopReason::Drained);
                }
            }

            if n > 0 && !owners.is_empty() {
                let readable: Vec<usize> = owners
                    .iter()
                    .enumerate()
                    .filter(|&(k, _)| (fds[k + 1].revents & libc::POLLIN) != 0)
                    .map(|(_, &i)| i)
                    .collect();
                self.fork_readable(&readable, now);
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
        let base = Instant::now();
        assert_eq!(poll_timeout_ms(base + Duration::from_micros(1500), base), 2);
        assert_eq!(poll_timeout_ms(base + Duration::from_nanos(1), base), 1);
        assert_eq!(poll_timeout_ms(base + Duration::from_millis(3), base), 3);
        assert_eq!(poll_timeout_ms(base, base + Duration::from_millis(5)), 0);
    }
}
