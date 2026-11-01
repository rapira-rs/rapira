//! One pool: its workers, the respawn with backoff, the request watchdog, the memory limit and the status log.

use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::time::{Duration, Instant};

use libc::c_int;
use rapira_scoreboard::{SLOT_ACTIVE, SLOT_FREE, SLOT_IDLE, SharedSlot, now_millis};

use crate::PoolConfig;
use crate::process::{ExitVerdict, Forker, SlotState, WorkerProc, kill, rss};

/// One plugin's workers.
pub(crate) struct Pool {
    /// Index into `MasterConfig::pools`; the fork passes it as `WorkerEnv::pool`.
    index: usize,
    pub cfg: PoolConfig,
    /// Sub-view of the shared board; every slot index in this struct is local to it.
    board: &'static [SharedSlot],
    /// The live workers, one per forked pid, until the reap.
    pub procs: Vec<WorkerProc>,
    /// Respawn state per slot, local indices as in `board`.
    slot_states: Vec<SlotState>,
}

impl Pool {
    pub(crate) fn new(index: usize, cfg: PoolConfig, board: &'static [SharedSlot]) -> Pool {
        Pool {
            index,
            cfg,
            procs: Vec::new(),
            slot_states: vec![SlotState::default(); board.len()],
            board,
        }
    }

    /// Requests completed without error: Acquire pairs with the worker's Release on `handled` (stored after `errors`) so a shed 503 never counts as a success.
    fn total_successful(&self) -> u64 {
        let mut total: u64 = 0;
        for s in self.board {
            let handled = s.handled.load(Acquire);
            let errors = s.errors.load(Relaxed);
            total += handled.saturating_sub(errors);
        }
        total
    }

    fn spawn_into(&mut self, slot: usize, now: Instant, spawner: &mut Forker) {
        self.board[slot].set_starting();
        match spawner.spawn(self.index, &self.board[slot]) {
            Ok(pid) => {
                self.board[slot].pid.store(pid as u32, Relaxed);
                self.procs.push(WorkerProc {
                    pid,
                    slot,
                    spawned_at: now,
                    timeout_kill: false,
                    memory_drain: false,
                });
            }
            Err(e) => {
                tracing::error!(
                    target: "master",
                    "{} pool: spawn failed for slot {slot}: {e}",
                    self.cfg.name
                );
                self.board[slot].clear();
                self.slot_states[slot].schedule_backoff(Duration::ZERO, now);
            }
        }
    }

    /// Forks a worker into every slot. Runs once at the start, while every slot is free.
    pub(crate) fn spawn_all(&mut self, now: Instant, spawner: &mut Forker) {
        for slot in 0..self.board.len() {
            self.spawn_into(slot, now, spawner);
        }
    }

    /// Sends SIGQUIT to every worker and cancels every scheduled respawn: no worker starts during a stop.
    pub(crate) fn begin_stop(&mut self) {
        self.signal_all(libc::SIGQUIT);
        for s in &mut self.slot_states {
            s.respawn_at = None;
        }
    }

    pub(crate) fn signal_all(&self, sig: c_int) {
        for p in &self.procs {
            kill(p.pid, sig);
        }
    }

    /// A boot failure only for a pool that never served and has no serving worker: a worker that never boots must not take down a pool that booted.
    pub(crate) fn on_child_exit(
        &mut self,
        w: WorkerProc,
        verdict: ExitVerdict,
        now: Instant,
        stopping: bool,
    ) -> anyhow::Result<()> {
        let slot = w.slot;
        let lived = now.saturating_duration_since(w.spawned_at);
        count_exit(&self.board[slot], verdict);
        self.board[slot].clear();

        if stopping {
            return Ok(());
        }
        // A recycle shows that an app of the pool booted once, so a later boot failure is not a pool boot failure.
        if verdict == ExitVerdict::Unhealthy
            && self.total_successful() == 0
            && !self.board.iter().any(SharedSlot::serving)
            && self.board.iter().all(|s| s.recycles.load(Relaxed) == 0)
        {
            anyhow::bail!(
                "{} pool: worker {} exited unhealthy before the pool served any request",
                self.cfg.name,
                w.pid
            );
        }
        match verdict {
            ExitVerdict::Drain | ExitVerdict::TimeoutKill => {
                self.slot_states[slot].schedule_immediate(now);
            }
            ExitVerdict::Unhealthy | ExitVerdict::Crash => {
                self.slot_states[slot].schedule_backoff(lived, now);
            }
        }
        Ok(())
    }

    /// Sends SIGTERM after the request timeout. A later tick sends SIGKILL if the worker stays active. The Acquire load orders the timestamp read.
    fn watchdog_tick(&mut self) {
        let limit = self.cfg.request_terminate_timeout;
        if limit.is_zero() {
            return;
        }
        let now_ms = now_millis();
        for p in self.procs.iter_mut() {
            let s = &self.board[p.slot];
            if s.state.load(Acquire) != SLOT_ACTIVE {
                continue;
            }
            let age_ms = u128::from(now_ms.saturating_sub(s.last_activity_ms.load(Relaxed)));
            if age_ms < limit.as_millis() {
                continue;
            }
            if p.timeout_kill {
                kill(p.pid, libc::SIGKILL);
            } else {
                tracing::warn!(
                    target: "master",
                    "{} pool: worker {} exceeded {}.pool.request_terminate_timeout_secs ({}s); terminating",
                    self.cfg.name,
                    p.pid,
                    self.cfg.name,
                    limit.as_secs()
                );
                kill(p.pid, libc::SIGTERM);
                p.timeout_kill = true;
            }
        }
    }

    /// Sends SIGQUIT once to a worker whose RSS is above the limit. The worker drains, and `on_child_exit` schedules the replacement without backoff.
    fn memory_tick(&mut self) {
        let limit_mb = self.cfg.max_memory_mb;
        if limit_mb == 0 {
            return;
        }
        for p in self.procs.iter_mut().filter(|p| !p.memory_drain) {
            let Some(bytes) = rss(p.pid) else {
                continue;
            };
            if bytes <= limit_mb.saturating_mul(1 << 20) {
                continue;
            }
            tracing::warn!(
                target: "master",
                "{} pool: worker {} uses {} MiB, above {}.pool.max_memory_mb ({}); draining",
                self.cfg.name,
                p.pid,
                bytes.div_ceil(1 << 20),
                self.cfg.name,
                limit_mb
            );
            kill(p.pid, libc::SIGQUIT);
            p.memory_drain = true;
        }
    }

    pub(crate) fn maintenance_tick(&mut self) {
        self.watchdog_tick();
        self.memory_tick();
    }

    pub(crate) fn fire_due(&mut self, now: Instant, spawner: &mut Forker) {
        for slot in 0..self.slot_states.len() {
            if let Some(t) = self.slot_states[slot].respawn_at
                && now >= t
            {
                self.slot_states[slot].respawn_at = None;
                self.spawn_into(slot, now, spawner);
            }
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.slot_states.iter().filter_map(|s| s.respawn_at).min()
    }

    /// Logs the pool counts and every used slot at info level, on USR1.
    pub(crate) fn log_status(&self) {
        let idle = self
            .board
            .iter()
            .filter(|s| s.state.load(Relaxed) == SLOT_IDLE)
            .count();
        tracing::info!(
            target: "master",
            "status: {} pool: {} running, {} idle",
            self.cfg.name,
            self.procs.len(),
            idle
        );
        for (i, s) in self.board.iter().enumerate() {
            let state = s.state.load(Relaxed);
            if state == SLOT_FREE {
                continue;
            }
            let pid = s.pid.load(Relaxed);
            tracing::info!(
                target: "master",
                "  slot {} pid {} state {} handled {} errors {} recycles {}",
                i,
                pid,
                state,
                s.handled.load(Relaxed),
                s.errors.load(Relaxed),
                s.recycles.load(Relaxed)
            );
        }
    }
}

/// Counts the exit in the slot of the worker. It runs before `clear`, while no worker owns the slot.
fn count_exit(slot: &SharedSlot, verdict: ExitVerdict) {
    let counter = match verdict {
        ExitVerdict::Drain => &slot.exits_drained,
        ExitVerdict::Unhealthy => &slot.exits_unhealthy,
        ExitVerdict::TimeoutKill => &slot.exits_timeout,
        ExitVerdict::Crash => &slot.exits_crashed,
    };
    counter.fetch_add(1, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::bury;

    fn test_pool(processes: usize) -> Pool {
        let board = rapira_scoreboard::create(processes).unwrap();
        let cfg = PoolConfig {
            name: "http",
            processes,
            request_terminate_timeout: Duration::ZERO,
            max_memory_mb: 0,
        };
        Pool::new(0, cfg, board)
    }

    #[test]
    fn bury_routes_a_pid_to_the_pool_that_owns_it() {
        let mut pools = [test_pool(1), test_pool(1)];
        let at = Instant::now();
        pools[0].procs.push(WorkerProc {
            pid: 2_000_000_001,
            slot: 0,
            spawned_at: at,
            timeout_kill: false,
            memory_drain: false,
        });
        pools[1].procs.push(WorkerProc {
            pid: 2_000_000_002,
            slot: 0,
            spawned_at: at,
            timeout_kill: false,
            memory_drain: false,
        });

        assert!(bury(&mut pools, 2_000_000_009, 0).is_none());
        // Raw wait status: exit code 89 in bits 8..16.
        let buried =
            bury(&mut pools, 2_000_000_002, 89 << 8).expect("the second pool owns the pid");

        assert_eq!(buried.pool, 1);
        assert_eq!(buried.worker.pid, 2_000_000_002);
        assert_eq!(buried.verdict, ExitVerdict::Unhealthy);
        assert_eq!(pools[0].procs.len(), 1, "the other pool is untouched");
        assert!(pools[1].procs.is_empty());
    }

    fn exits(slot: &SharedSlot) -> [u64; 4] {
        [
            slot.exits_drained.load(Relaxed),
            slot.exits_unhealthy.load(Relaxed),
            slot.exits_timeout.load(Relaxed),
            slot.exits_crashed.load(Relaxed),
        ]
    }

    #[test]
    fn each_exit_verdict_counts_in_its_own_field() {
        struct Case {
            name: &'static str,
            verdict: ExitVerdict,
            want: [u64; 4],
        }
        let cases = [
            Case {
                name: "drain",
                verdict: ExitVerdict::Drain,
                want: [1, 0, 0, 0],
            },
            Case {
                name: "unhealthy",
                verdict: ExitVerdict::Unhealthy,
                want: [0, 1, 0, 0],
            },
            Case {
                name: "timeout kill",
                verdict: ExitVerdict::TimeoutKill,
                want: [0, 0, 1, 0],
            },
            Case {
                name: "crash",
                verdict: ExitVerdict::Crash,
                want: [0, 0, 0, 1],
            },
        ];
        for case in cases {
            let board = rapira_scoreboard::create(1).unwrap();
            count_exit(&board[0], case.verdict);
            assert_eq!(exits(&board[0]), case.want, "{}", case.name);
        }
    }
}
