use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::time::{Duration, Instant};

use libc::c_int;
use rapira_scoreboard::{SLOT_ACTIVE, SLOT_FREE, SLOT_IDLE, SharedSlot, now_millis};

use crate::PoolConfig;
use crate::pctl::KillPhase;
use crate::process::{ExitVerdict, Forker, SlotState, WorkerProc, kill};

/// Re-check cadence for the overlap reload gate; the total wait is bounded by `process_control_timeout`.
const RELOAD_GATE_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReloadPhase {
    /// Gate: the replacement in `slot` must report IDLE or ACTIVE before the next old worker drains; `until` forces past a stuck one.
    Await { slot: usize, until: Instant },
    Drain {
        draining: libc::pid_t,
        phase: KillPhase,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reload {
    pub phase: ReloadPhase,
    /// Next gate probe or drain escalation.
    pub deadline: Instant,
}

/// One plugin's workers.
pub(crate) struct Pool {
    pub index: usize,
    pub cfg: PoolConfig,
    /// Sub-view of the shared board; every slot index in this struct is local to it.
    pub board: &'static [SharedSlot],
    pub procs: Vec<WorkerProc>,
    /// Respawn state per slot, local indices as in `board`.
    pub slot_states: Vec<SlotState>,
    pub generation: u32,
    pub control_timeout: Duration,
    /// This pool's overlap-reload chain; `None` once finished or never started.
    pub reload: Option<Reload>,
}

impl Pool {
    pub(crate) fn new(
        index: usize,
        cfg: PoolConfig,
        board: &'static [SharedSlot],
        control_timeout: Duration,
    ) -> Pool {
        Pool {
            index,
            cfg,
            procs: Vec::new(),
            slot_states: vec![SlotState::default(); board.len()],
            generation: 0,
            board,
            control_timeout,
            reload: None,
        }
    }

    /// Requests completed without error: Acquire pairs with the worker's Release on `handled` (stored after `errors`) so a shed 503 never counts as a success.
    fn total_successful(&self) -> u64 {
        self.board
            .iter()
            .map(|s| {
                let handled = s.handled.load(Acquire);
                let errors = s.errors.load(Relaxed);
                handled.saturating_sub(errors)
            })
            .sum()
    }

    fn find_spawn_slot(&self) -> Option<usize> {
        (0..self.slot_states.len()).find(|&i| {
            self.board[i].state.load(Relaxed) == SLOT_FREE
                && self.slot_states[i].respawn_at.is_none()
        })
    }

    fn has_old_gen(&self) -> bool {
        let cur = self.generation;
        self.procs.iter().any(|p| p.generation < cur)
    }

    fn spawn_into(&mut self, slot: usize, now: Instant, spawner: &mut Forker<'_>) {
        self.board[slot].set_starting();
        let generation = self.generation;
        match spawner.spawn(self.index, &self.board[slot]) {
            Ok(pid) => {
                self.board[slot].pid.store(pid as u32, Relaxed);
                self.procs.push(WorkerProc {
                    pid,
                    slot,
                    generation,
                    spawned_at: now,
                    timeout_kill: false,
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

    fn spawn_up_to(&mut self, n: usize, now: Instant, spawner: &mut Forker<'_>) {
        for _ in 0..n {
            let Some(slot) = self.find_spawn_slot() else {
                return;
            };
            self.spawn_into(slot, now, spawner);
        }
    }

    pub(crate) fn begin_stop(&mut self) {
        self.signal_all(libc::SIGQUIT);
        for s in &mut self.slot_states {
            s.respawn_at = None;
        }
        self.reload = None;
    }

    pub(crate) fn signal_all(&self, sig: c_int) {
        for p in &self.procs {
            kill(p.pid, sig);
        }
    }

    /// Overlap reload: spawn one current-gen worker as headroom and gate on it serving before any old worker is drained, so capacity never dips.
    pub(crate) fn begin_reload(&mut self, now: Instant, spawner: &mut Forker<'_>) {
        self.generation += 1;
        let slot = if self.has_old_gen() {
            self.find_spawn_slot()
        } else {
            None
        };
        self.reload_enter_await(slot, now, spawner);
    }

    /// Without a free slot, spawns no replacement and drains the next old worker directly.
    fn reload_enter_await(&mut self, slot: Option<usize>, now: Instant, spawner: &mut Forker<'_>) {
        match slot {
            Some(s) => {
                self.spawn_into(s, now, spawner);
                self.reload = Some(Reload {
                    phase: ReloadPhase::Await {
                        slot: s,
                        until: now + self.control_timeout,
                    },
                    deadline: now + RELOAD_GATE_POLL,
                });
            }
            None => self.reload_quit_next(now),
        }
    }

    fn reload_quit_next(&mut self, now: Instant) {
        let cur = self.generation;
        let target = self
            .procs
            .iter()
            .filter(|p| p.generation < cur)
            .min_by_key(|p| p.spawned_at)
            .map(|p| p.pid);
        match target {
            Some(pid) => {
                kill(pid, libc::SIGQUIT);
                self.reload = Some(Reload {
                    phase: ReloadPhase::Drain {
                        draining: pid,
                        phase: KillPhase::Quit,
                    },
                    deadline: now + self.control_timeout,
                });
            }
            None => self.reload = None,
        }
    }

    /// In the Await gate: re-probe the replacement and force past a stuck one at the safety cap. In Drain: QUIT to TERM to KILL against the draining worker.
    fn on_reload_deadline(&mut self, now: Instant) {
        let Some(reload) = self.reload else {
            return;
        };
        match reload.phase {
            ReloadPhase::Await { slot, until } => {
                if self.board[slot].serving() {
                    self.reload_quit_next(now);
                } else if now >= until {
                    tracing::warn!(
                        target: "master",
                        "{} pool: reload replacement slot {slot} not serving within the control timeout; proceeding",
                        self.cfg.name
                    );
                    self.reload_quit_next(now);
                } else {
                    self.reload = Some(Reload {
                        deadline: now + RELOAD_GATE_POLL,
                        ..reload
                    });
                }
            }
            ReloadPhase::Drain {
                draining,
                mut phase,
            } => {
                let sig = phase.advance();
                self.reload = Some(Reload {
                    phase: ReloadPhase::Drain { draining, phase },
                    deadline: now + Duration::from_secs(1),
                });
                kill(draining, sig);
            }
        }
    }

    /// Failboot only for a gen-0 worker in a pool that never served and has no serving worker: a reload replacement dying unhealthy must not take down the running pool, and a worker that never boots must not take down a pool that booted.
    pub(crate) fn on_child_exit(
        &mut self,
        w: WorkerProc,
        verdict: ExitVerdict,
        now: Instant,
        stopping: bool,
        spawner: &mut Forker<'_>,
    ) -> anyhow::Result<()> {
        let slot = w.slot;
        let lived = now.saturating_duration_since(w.spawned_at);
        count_exit(&self.board[slot], verdict);
        self.board[slot].clear();

        if let Some(Reload {
            phase: ReloadPhase::Drain { draining, .. },
            ..
        }) = self.reload
            && draining == w.pid
        {
            if self.has_old_gen() {
                self.reload_enter_await(Some(slot), now, spawner);
            } else {
                self.reload = None;
            }
            return Ok(());
        }
        if stopping {
            return Ok(());
        }

        match verdict {
            ExitVerdict::Recycle | ExitVerdict::Drain | ExitVerdict::TimeoutKill => {
                self.slot_states[slot].schedule_immediate(now);
            }
            ExitVerdict::Unhealthy => {
                if w.generation == 0
                    && self.total_successful() == 0
                    && !self.board.iter().any(SharedSlot::serving)
                {
                    anyhow::bail!(
                        "{} pool: worker {} exited unhealthy before the pool served any request",
                        self.cfg.name,
                        w.pid
                    );
                }
                self.slot_states[slot].schedule_backoff(lived, now);
            }
            ExitVerdict::Crash => {
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

    /// Nothing runs while the master stops; the stop escalation bounds every worker. The refill pauses while this pool drains a reload chain: it would race the chain for the slot it just freed. The request watchdog keeps running during the reload.
    pub(crate) fn maintenance_tick(
        &mut self,
        now: Instant,
        stopping: bool,
        spawner: &mut Forker<'_>,
    ) {
        if stopping {
            return;
        }
        self.watchdog_tick();
        if self.reload.is_some() {
            return;
        }
        self.refill(now, spawner);
    }

    pub(crate) fn refill(&mut self, now: Instant, spawner: &mut Forker<'_>) {
        let running = self.procs.len();
        let pending = (0..self.slot_states.len())
            .filter(|&i| self.slot_states[i].respawn_at.is_some())
            .count();
        let committed = running + pending;
        self.spawn_up_to(self.cfg.processes.saturating_sub(committed), now, spawner);
    }

    pub(crate) fn fire_due(&mut self, now: Instant, spawner: &mut Forker<'_>) {
        if let Some(r) = self.reload
            && now >= r.deadline
        {
            self.on_reload_deadline(now);
        }
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
        self.slot_states
            .iter()
            .filter_map(|s| s.respawn_at)
            .chain(self.reload.map(|r| r.deadline))
            .min()
    }

    pub(crate) fn log_status(&self) {
        let idle = self
            .board
            .iter()
            .filter(|s| s.state.load(Relaxed) == SLOT_IDLE)
            .count();
        tracing::info!(
            target: "master",
            "status: {} pool: {} running, {} idle, generation {}",
            self.cfg.name,
            self.procs.len(),
            idle,
            self.generation
        );
        for (i, s) in self.board.iter().enumerate() {
            let state = s.state.load(Relaxed);
            let pid = s.pid.load(Relaxed);
            if state == SLOT_FREE && pid == 0 {
                continue;
            }
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
        ExitVerdict::Recycle => &slot.exits_recycled,
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
        let board = rapira_scoreboard::create(processes * 2).unwrap();
        let cfg = PoolConfig {
            name: "http",
            processes,
            request_terminate_timeout: Duration::ZERO,
        };
        Pool::new(0, cfg, board, Duration::from_secs(30))
    }

    #[test]
    fn next_deadline_is_the_earliest_of_reload_and_respawns() {
        let mut p = test_pool(3);
        let t0 = Instant::now();
        assert_eq!(p.next_deadline(), None);

        p.slot_states[1].respawn_at = Some(t0 + Duration::from_millis(700));
        p.slot_states[2].respawn_at = Some(t0 + Duration::from_millis(400));
        p.reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 0,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0 + Duration::from_millis(500),
        });
        assert_eq!(p.next_deadline(), Some(t0 + Duration::from_millis(400)));

        p.slot_states[2].respawn_at = None;
        assert_eq!(p.next_deadline(), Some(t0 + Duration::from_millis(500)));
    }

    #[test]
    fn bury_routes_a_pid_to_the_pool_that_owns_it() {
        let mut pools = [test_pool(1), test_pool(1)];
        let at = Instant::now();
        pools[0].procs.push(WorkerProc {
            pid: 2_000_000_001,
            slot: 0,
            generation: 0,
            spawned_at: at,
            timeout_kill: false,
        });
        pools[1].procs.push(WorkerProc {
            pid: 2_000_000_002,
            slot: 1,
            generation: 3,
            spawned_at: at,
            timeout_kill: false,
        });

        assert!(bury(&mut pools, 2_000_000_009, 0).is_none());
        // Raw wait status: exit code 89 in bits 8..16.
        let (pool, w, verdict) =
            bury(&mut pools, 2_000_000_002, 89 << 8).expect("the second pool owns the pid");

        assert_eq!(pool, 1);
        assert_eq!((w.slot, w.generation), (1, 3));
        assert_eq!(verdict, ExitVerdict::Unhealthy);
        assert_eq!(pools[0].procs.len(), 1, "the other pool is untouched");
        assert!(pools[1].procs.is_empty());
    }

    fn exits(slot: &SharedSlot) -> [u64; 5] {
        [
            slot.exits_drained.load(Relaxed),
            slot.exits_recycled.load(Relaxed),
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
            want: [u64; 5],
        }
        let cases = [
            Case {
                name: "drain",
                verdict: ExitVerdict::Drain,
                want: [1, 0, 0, 0, 0],
            },
            Case {
                name: "recycle",
                verdict: ExitVerdict::Recycle,
                want: [0, 1, 0, 0, 0],
            },
            Case {
                name: "unhealthy",
                verdict: ExitVerdict::Unhealthy,
                want: [0, 0, 1, 0, 0],
            },
            Case {
                name: "timeout kill",
                verdict: ExitVerdict::TimeoutKill,
                want: [0, 0, 0, 1, 0],
            },
            Case {
                name: "crash",
                verdict: ExitVerdict::Crash,
                want: [0, 0, 0, 0, 1],
            },
        ];
        for case in cases {
            let board = rapira_scoreboard::create(1).unwrap();
            count_exit(&board[0], case.verdict);
            assert_eq!(exits(&board[0]), case.want, "{}", case.name);
        }
    }
}
