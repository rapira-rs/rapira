use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::time::{Duration, Instant};

use libc::c_int;
use rapira_scoreboard::{SLOT_ACTIVE, SLOT_FREE, SLOT_IDLE, SLOT_STARTING, Scoreboard, now_millis};

use crate::pctl::KillPhase;
use crate::process::{ExitVerdict, Forker, KillIntent, ProcTable, WorkerProc, kill};
use crate::scaling::{DynAction, DynInput, dynamic_start_count, dynamic_tick, ondemand_armed};
use crate::{PoolConfig, Scaling};

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
    pub board: Scoreboard,
    pub table: ProcTable,
    pub control_timeout: Duration,
    pub spawn_rate: u32,
    pub warned_max_children: bool,
    /// Latched history: scoreboard counters cannot carry it, a replacement `bind()` zeroes a slot's served count.
    pub ever_served: bool,
    /// This pool's overlap-reload chain; `None` once finished or never started.
    pub reload: Option<Reload>,
}

impl Pool {
    pub(crate) fn new(
        index: usize,
        cfg: PoolConfig,
        board: Scoreboard,
        control_timeout: Duration,
    ) -> Pool {
        let table = ProcTable::new(board.nslots());
        Pool {
            index,
            cfg,
            board,
            table,
            control_timeout,
            spawn_rate: 1,
            warned_max_children: false,
            ever_served: false,
            reload: None,
        }
    }

    fn idle_count(&self) -> usize {
        self.board
            .slots()
            .iter()
            .filter(|s| s.state.load(Relaxed) == SLOT_IDLE)
            .count()
    }

    fn starting_count(&self) -> usize {
        self.board
            .slots()
            .iter()
            .filter(|s| s.state.load(Relaxed) == SLOT_STARTING)
            .count()
    }

    /// Requests completed without error: Acquire pairs with the worker's Release on `handled` (stored after `errors`) so a shed 503 never counts as a success.
    fn total_successful(&self) -> u64 {
        self.board
            .slots()
            .iter()
            .map(|s| {
                let handled = s.handled.load(Acquire);
                let errors = s.errors.load(Relaxed);
                handled.saturating_sub(errors)
            })
            .sum()
    }

    /// Must run before a replacement bind resets the slot counters.
    fn latch_served(&mut self) {
        if !self.ever_served && self.total_successful() > 0 {
            self.ever_served = true;
        }
    }

    fn slot_is_free(&self, i: usize) -> bool {
        self.board.slot(i).state.load(Relaxed) == SLOT_FREE
    }

    // Acquire: ondemand maintenance pairs this with a later timestamp read
    fn slot_is_idle(&self, i: usize) -> bool {
        self.board.slot(i).state.load(Acquire) == SLOT_IDLE
    }

    /// Serving is IDLE or ACTIVE: under load a replacement may never be observed IDLE between requests.
    fn slot_is_serving(&self, i: usize) -> bool {
        let state = self.board.slot(i).state.load(Relaxed);
        state == SLOT_IDLE || state == SLOT_ACTIVE
    }

    fn find_spawn_slot(&self) -> Option<usize> {
        (0..self.table.slots.len())
            .find(|&i| self.slot_is_free(i) && self.table.slots[i].respawn_at.is_none())
    }

    fn oldest_idle_pid(&self) -> Option<libc::pid_t> {
        self.table
            .procs
            .iter()
            .filter(|p| self.slot_is_idle(p.slot))
            .min_by_key(|p| p.spawned_at)
            .map(|p| p.pid)
    }

    fn has_old_gen(&self) -> bool {
        let cur = self.table.generation;
        self.table.procs.iter().any(|p| p.generation < cur)
    }

    fn spawn_into(&mut self, slot: usize, now: Instant, spawner: &mut Forker<'_>) {
        self.board.set_starting(slot);
        let generation = self.table.generation;
        match spawner.spawn(self.index, self.board.slot(slot)) {
            Ok(pid) => self.table.procs.push(WorkerProc {
                pid,
                slot,
                generation,
                spawned_at: now,
                kill_intent: None,
            }),
            Err(e) => {
                tracing::error!(
                    target: "master",
                    "{} pool: spawn failed for slot {slot}: {e}",
                    self.cfg.name
                );
                self.board.clear(slot);
                self.table.slots[slot].schedule_backoff(Duration::ZERO, now);
            }
        }
    }

    pub(crate) fn fork_initial(&mut self, now: Instant, spawner: &mut Forker<'_>) {
        let count: usize = match self.cfg.scaling {
            Scaling::Static => self.cfg.processes,
            Scaling::Dynamic {
                min_spare,
                max_spare,
            } => dynamic_start_count(min_spare, max_spare, self.cfg.processes),
            Scaling::Ondemand => 0,
        };
        for _ in 0..count {
            match self.find_spawn_slot() {
                Some(slot) => self.spawn_into(slot, now, spawner),
                None => break,
            }
        }
    }

    /// Arm only when a fork could land: a readable level-triggered listener would otherwise busy-spin poll through the backoff window.
    pub(crate) fn armed(&self, stopping: bool) -> bool {
        if !matches!(self.cfg.scaling, Scaling::Ondemand) {
            return false;
        }
        ondemand_armed(
            !stopping,
            self.table.running(),
            self.cfg.processes,
            self.idle_count(),
            self.starting_count(),
        ) && self.find_spawn_slot().is_some()
    }

    pub(crate) fn ondemand_fork_one(&mut self, now: Instant, spawner: &mut Forker<'_>) {
        if let Some(slot) = self.find_spawn_slot() {
            self.spawn_into(slot, now, spawner);
        }
    }

    pub(crate) fn begin_stop(&mut self) {
        self.signal_all(libc::SIGQUIT);
        for s in &mut self.table.slots {
            s.cancel_respawn();
        }
        self.reload = None;
    }

    pub(crate) fn signal_all(&self, sig: c_int) {
        for p in &self.table.procs {
            kill(p.pid, sig);
        }
    }

    /// Overlap reload: spawn one current-gen worker as headroom and gate on it serving before any old worker is drained, so capacity never dips.
    pub(crate) fn begin_reload(&mut self, now: Instant, spawner: &mut Forker<'_>) {
        self.table.generation += 1;
        let slot = if self.has_old_gen() {
            self.find_spawn_slot()
        } else {
            None
        };
        self.reload_enter_await(slot, now, spawner);
    }

    /// Ondemand (or no free slot) spawns no replacement and drains the next old worker directly: replacements come from demand.
    fn reload_enter_await(&mut self, slot: Option<usize>, now: Instant, spawner: &mut Forker<'_>) {
        match slot {
            Some(s) if !matches!(self.cfg.scaling, Scaling::Ondemand) => {
                self.spawn_into(s, now, spawner);
                self.reload = Some(Reload {
                    phase: ReloadPhase::Await {
                        slot: s,
                        until: now + self.control_timeout,
                    },
                    deadline: now + RELOAD_GATE_POLL,
                });
            }
            _ => self.reload_quit_next(now),
        }
    }

    fn reload_quit_next(&mut self, now: Instant) {
        let cur = self.table.generation;
        let target = self
            .table
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
                if self.slot_is_serving(slot) {
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

    /// Failboot only for a gen-0 worker in a pool that never served: a reload replacement dying unhealthy must not take down the running pool.
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
        self.latch_served();
        self.board.clear(slot);

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
            ExitVerdict::IdleKill => {}
            ExitVerdict::Recycle | ExitVerdict::Drain | ExitVerdict::TimeoutKill => {
                self.table.slots[slot].schedule_immediate(now);
                self.apply_respawn_gate(slot);
            }
            ExitVerdict::Unhealthy => {
                if w.generation == 0 && !self.ever_served {
                    anyhow::bail!(
                        "{} pool: worker {} exited unhealthy before the pool served any request",
                        self.cfg.name,
                        w.pid
                    );
                }
                self.table.slots[slot].schedule_backoff(lived, now);
            }
            ExitVerdict::Crash => {
                self.table.slots[slot].schedule_backoff(lived, now);
            }
        }
        Ok(())
    }

    /// Ondemand re-forks from demand; crash and unhealthy backoff deadlines are kept as fork suppression until they expire, throttling a fork-crash loop.
    fn apply_respawn_gate(&mut self, slot: usize) {
        if matches!(self.cfg.scaling, Scaling::Ondemand) {
            self.table.slots[slot].cancel_respawn();
        }
    }

    fn idle_kill_pid(&mut self, pid: libc::pid_t) {
        if let Some(p) = self.table.procs.iter_mut().find(|p| p.pid == pid) {
            if p.kill_intent == Some(KillIntent::Idle) {
                kill(pid, libc::SIGKILL);
            } else {
                kill(pid, libc::SIGQUIT);
                p.kill_intent = Some(KillIntent::Idle);
            }
        }
    }

    /// Sends SIGTERM after the request timeout. A later tick sends SIGKILL if the worker stays active. The Acquire load orders the timestamp read.
    fn watchdog_tick(&mut self) {
        let limit = self.cfg.request_terminate_timeout;
        if limit.is_zero() {
            return;
        }
        let now_ms = now_millis();
        for p in self.table.procs.iter_mut() {
            let s = self.board.slot(p.slot);
            if s.state.load(Acquire) != SLOT_ACTIVE {
                continue;
            }
            let age_ms = u128::from(now_ms.saturating_sub(s.last_activity_ms.load(Relaxed)));
            if age_ms < limit.as_millis() {
                continue;
            }
            if p.kill_intent == Some(KillIntent::Timeout) {
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
                p.kill_intent = Some(KillIntent::Timeout);
            }
        }
    }

    /// Nothing runs while the master stops; the stop escalation bounds every worker. Scaling pauses while this pool drains a reload chain: a refill would race the chain for the slot it just freed. The served latch and the request watchdog keep running during the reload.
    pub(crate) fn maintenance_tick(
        &mut self,
        now: Instant,
        stopping: bool,
        spawner: &mut Forker<'_>,
    ) {
        if stopping {
            return;
        }
        self.latch_served();
        self.watchdog_tick();
        if self.reload.is_some() {
            return;
        }
        match self.cfg.scaling {
            Scaling::Static => self.static_refill(now, spawner),
            Scaling::Dynamic {
                min_spare,
                max_spare,
            } => self.dynamic_maintenance(min_spare, max_spare, now, spawner),
            Scaling::Ondemand => self.ondemand_maintenance(),
        }
    }

    fn static_refill(&mut self, now: Instant, spawner: &mut Forker<'_>) {
        let running = self.table.running();
        let pending = (0..self.table.slots.len())
            .filter(|&i| self.table.slots[i].respawn_at.is_some())
            .count();
        let committed = running + pending;
        let target = self.cfg.processes;
        for _ in committed..target {
            match self.find_spawn_slot() {
                Some(slot) => self.spawn_into(slot, now, spawner),
                None => break,
            }
        }
    }

    fn dynamic_maintenance(
        &mut self,
        min_spare: usize,
        max_spare: usize,
        now: Instant,
        spawner: &mut Forker<'_>,
    ) {
        let inp = DynInput {
            idle: self.idle_count(),
            running: self.table.running(),
            min_spare,
            max_spare,
            max_children: self.cfg.processes,
        };
        match dynamic_tick(&inp, &mut self.spawn_rate) {
            DynAction::KillOldestIdle => {
                if let Some(pid) = self.oldest_idle_pid() {
                    self.idle_kill_pid(pid);
                }
            }
            DynAction::Spawn(n) => {
                for _ in 0..n {
                    match self.find_spawn_slot() {
                        Some(slot) => self.spawn_into(slot, now, spawner),
                        None => break,
                    }
                }
                self.warned_max_children = false;
            }
            DynAction::ReachedMaxChildren => {
                if !self.warned_max_children {
                    self.warned_max_children = true;
                    tracing::warn!(
                        target: "master",
                        "reached {}.pool.processes ceiling ({}), consider raising it",
                        self.cfg.name,
                        self.cfg.processes
                    );
                }
            }
            DynAction::Steady => {}
        }
    }

    /// Trims the idle worker with the stalest activity: by process age, a busy older worker could shield a long-expired younger one indefinitely.
    fn ondemand_maintenance(&mut self) {
        let target = self
            .table
            .procs
            .iter()
            .filter(|p| self.slot_is_idle(p.slot))
            .map(|p| {
                let s = self.board.slot(p.slot);
                (p.pid, s.last_activity_ms.load(Relaxed))
            })
            .min_by_key(|&(_, last)| last);
        let Some((pid, last)) = target else {
            return;
        };
        let age_ms = u128::from(now_millis().saturating_sub(last));
        if age_ms >= self.cfg.process_idle_timeout.as_millis() {
            self.idle_kill_pid(pid);
        }
    }

    pub(crate) fn fire_due(&mut self, now: Instant, spawner: &mut Forker<'_>) {
        if let Some(r) = self.reload
            && now >= r.deadline
        {
            self.on_reload_deadline(now);
        }
        for slot in 0..self.table.slots.len() {
            if let Some(t) = self.table.slots[slot].respawn_at
                && now >= t
            {
                self.table.slots[slot].cancel_respawn();
                if !matches!(self.cfg.scaling, Scaling::Ondemand) {
                    self.spawn_into(slot, now, spawner);
                }
            }
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.table
            .slots
            .iter()
            .filter_map(|s| s.respawn_at)
            .chain(self.reload.map(|r| r.deadline))
            .min()
    }

    pub(crate) fn log_status(&self) {
        tracing::info!(
            target: "master",
            "status: {} pool: {} running, {} idle, generation {}",
            self.cfg.name,
            self.table.running(),
            self.idle_count(),
            self.table.generation
        );
        for s in self.board.snapshot_slots() {
            tracing::info!(
                target: "master",
                "  slot {} pid {} state {} handled {} errors {} recycles {}",
                s.id, s.pid, s.state, s.handled, s.errors, s.recycles
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sentinel pid above PID_MAX_LIMIT: no live process holds it.
    const P_OLD0: libc::pid_t = 2_000_000_001;

    fn test_pool(processes: usize, scaling: Scaling) -> Pool {
        let board = Scoreboard::create(processes * 2).unwrap();
        let cfg = PoolConfig {
            name: "http",
            processes,
            scaling,
            process_idle_timeout: Duration::from_secs(10),
            request_terminate_timeout: Duration::ZERO,
            listeners: Vec::new(),
        };
        Pool::new(0, cfg, board, Duration::from_secs(30))
    }

    #[test]
    fn ondemand_arms_only_when_a_fork_can_land() {
        let mut p = test_pool(1, Scaling::Ondemand);
        assert!(p.armed(false));

        let t0 = Instant::now();
        for s in &mut p.table.slots {
            s.schedule_backoff(Duration::ZERO, t0);
        }
        assert!(!p.armed(false));
    }

    #[test]
    fn ondemand_stays_armed_while_this_pool_reloads() {
        let mut p = test_pool(1, Scaling::Ondemand);
        let t0 = Instant::now();
        p.reload = Some(Reload {
            phase: ReloadPhase::Drain {
                draining: P_OLD0,
                phase: KillPhase::Quit,
            },
            deadline: t0 + Duration::from_secs(1),
        });
        assert!(p.armed(false));
        assert!(!p.armed(true));
    }

    /// Static and dynamic workers accept in the children; the master watches only its self-pipe.
    #[test]
    fn non_ondemand_never_arms_listeners() {
        let p = test_pool(1, Scaling::Static);
        assert!(!p.armed(false));
    }

    #[test]
    fn next_deadline_is_the_earliest_of_reload_and_respawns() {
        let mut p = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        assert_eq!(p.next_deadline(), None);

        p.table.slots[1].respawn_at = Some(t0 + Duration::from_millis(700));
        p.table.slots[2].respawn_at = Some(t0 + Duration::from_millis(400));
        p.reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 0,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0 + Duration::from_millis(500),
        });
        assert_eq!(p.next_deadline(), Some(t0 + Duration::from_millis(400)));

        p.table.slots[2].respawn_at = None;
        assert_eq!(p.next_deadline(), Some(t0 + Duration::from_millis(500)));
    }
}
