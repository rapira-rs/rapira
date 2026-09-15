use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::time::{Duration, Instant};

use libc::c_int;
use rapira_scoreboard::{SLOT_ACTIVE, SLOT_FREE, SLOT_IDLE, SLOT_STARTING, Scoreboard, now_millis};

use crate::pctl::KillPhase;
use crate::process::{ExitVerdict, KillIntent, ProcTable, Spawner, WorkerProc, kill};
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

/// One plugin's workers. Every index here is local to this pool's board view.
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

    /// Must run before a slot clear or a replacement bind can overwrite the scoreboard counters.
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
        (0..self.table.slots.len()).find(|&i| {
            self.slot_is_free(i)
                && self.table.slots[i].respawn_at.is_none()
                && !self.table.has_proc(i)
        })
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

    fn spawn_into(&mut self, slot: usize, now: Instant, spawner: &mut dyn Spawner) {
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

    pub(crate) fn fork_initial(&mut self, now: Instant, spawner: &mut dyn Spawner) {
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
            !stopping && self.reload.is_none(),
            self.table.running(),
            self.cfg.processes,
            self.idle_count(),
            self.starting_count(),
        ) && self.find_spawn_slot().is_some()
    }

    pub(crate) fn ondemand_fork_one(&mut self, now: Instant, spawner: &mut dyn Spawner) {
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
    pub(crate) fn begin_reload(&mut self, now: Instant, spawner: &mut dyn Spawner) {
        self.table.generation += 1;
        let slot = if self.has_old_gen() {
            self.find_spawn_slot()
        } else {
            None
        };
        self.reload_enter_await(slot, now, spawner);
    }

    /// Ondemand (or no free slot) spawns no replacement and drains the next old worker directly: replacements come from demand.
    fn reload_enter_await(&mut self, slot: Option<usize>, now: Instant, spawner: &mut dyn Spawner) {
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

    fn reload_try_advance(&mut self, now: Instant) {
        if let Some(Reload {
            phase: ReloadPhase::Await { slot, .. },
            ..
        }) = self.reload
            && self.slot_is_serving(slot)
        {
            self.reload_quit_next(now);
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
                self.reload_try_advance(now);
                let Some(r) = self.reload.as_mut() else {
                    return;
                };
                if !matches!(r.phase, ReloadPhase::Await { .. }) {
                    return;
                }
                if now >= until {
                    tracing::warn!(
                        target: "master",
                        "{} pool: reload replacement slot {slot} not serving within the control timeout; proceeding",
                        self.cfg.name
                    );
                    self.reload_quit_next(now);
                } else {
                    r.deadline = now + RELOAD_GATE_POLL;
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
                if self.table.has_pid(draining) {
                    kill(draining, sig);
                }
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
        spawner: &mut dyn Spawner,
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

    /// Scaling pauses while this pool drains a reload chain: a refill would race the chain for the slot it just freed. The served latch and the request watchdog keep running.
    pub(crate) fn maintenance_tick(
        &mut self,
        now: Instant,
        stopping: bool,
        spawner: &mut dyn Spawner,
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

    fn static_refill(&mut self, now: Instant, spawner: &mut dyn Spawner) {
        let running = self.table.running();
        let pending = (0..self.table.slots.len())
            .filter(|&i| self.table.slots[i].respawn_at.is_some() && !self.table.has_proc(i))
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
        spawner: &mut dyn Spawner,
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

    pub(crate) fn fire_due(&mut self, now: Instant, spawner: &mut dyn Spawner) {
        if let Some(r) = self.reload
            && now >= r.deadline
        {
            self.on_reload_deadline(now);
        }
        for slot in 0..self.table.slots.len() {
            if let Some(t) = self.table.slots[slot].respawn_at
                && now >= t
                && !self.table.has_proc(slot)
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
impl Pool {
    pub(crate) fn push_proc(
        &mut self,
        pid: libc::pid_t,
        slot: usize,
        generation: u32,
        at: Instant,
    ) {
        self.table.procs.push(WorkerProc {
            pid,
            slot,
            generation,
            spawned_at: at,
            kill_intent: None,
        });
    }

    pub(crate) fn take_proc(&mut self, pid: libc::pid_t) -> WorkerProc {
        let i = self
            .table
            .procs
            .iter()
            .position(|p| p.pid == pid)
            .expect("the table holds the pid");
        self.table.procs.swap_remove(i)
    }

    pub(crate) fn set_slot(&self, slot: usize, state: u32) {
        self.board.slot(slot).state.store(state, Relaxed);
    }
}

/// Counts WARN events on the `master` target; hand-rolled because the crate depends only on the `tracing` facade.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct WarnCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl WarnCounter {
    pub(crate) fn count(&self) -> usize {
        self.0.load(Relaxed)
    }
}

#[cfg(test)]
impl tracing::Subscriber for WarnCounter {
    fn enabled(&self, md: &tracing::Metadata) -> bool {
        md.target() == "master" && *md.level() == tracing::Level::WARN
    }
    fn event(&self, _: &tracing::Event) {
        self.0.fetch_add(1, Relaxed);
    }
    fn new_span(&self, _: &tracing::span::Attributes) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{FakeSpawner, TestChild, dead_worker, wait_signal};
    use rapira_scoreboard::{SLOT_ACTIVE, SLOT_IDLE, SLOT_STARTING};
    use std::sync::atomic::Ordering::Relaxed;

    // Sentinel pids above PID_MAX_LIMIT: the QUIT/TERM/KILL these tests send resolve to ESRCH.
    const P_OLD0: libc::pid_t = 2_000_000_001;
    const P_OLD1: libc::pid_t = 2_000_000_002;
    const P_OLD2: libc::pid_t = 2_000_000_003;
    const P_NEW: libc::pid_t = 2_000_000_100;

    fn test_pool(processes: usize, scaling: Scaling) -> (Pool, FakeSpawner) {
        let board = Scoreboard::create(processes * 2).unwrap();
        let cfg = PoolConfig {
            name: "http",
            processes,
            scaling,
            process_idle_timeout: Duration::from_secs(10),
            request_terminate_timeout: Duration::ZERO,
            listeners: Vec::new(),
        };
        let pool = Pool::new(0, cfg, board, Duration::from_secs(30));
        (pool, FakeSpawner::new())
    }

    #[test]
    fn fork_initial_static_fills_processes() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        p.fork_initial(Instant::now(), &mut sp);

        assert_eq!(sp.calls.len(), 3);
        for (i, &(pool, view)) in sp.calls.iter().enumerate() {
            assert_eq!(pool, 0);
            assert!(
                std::ptr::eq(view, p.board.slot(i)),
                "spawn {i} took slot {i}"
            );
        }
        assert_eq!(p.table.running(), 3);
    }

    #[test]
    fn fork_initial_dynamic_starts_at_midpoint() {
        let (mut p, mut sp) = test_pool(
            10,
            Scaling::Dynamic {
                min_spare: 2,
                max_spare: 6,
            },
        );
        p.fork_initial(Instant::now(), &mut sp);
        assert_eq!(sp.calls.len(), 4);
    }

    #[test]
    fn fork_initial_ondemand_spawns_none() {
        let (mut p, mut sp) = test_pool(3, Scaling::Ondemand);
        p.fork_initial(Instant::now(), &mut sp);
        assert!(sp.calls.is_empty());
        assert_eq!(p.table.running(), 0);
    }

    #[test]
    fn begin_reload_spawns_one_replacement_and_awaits_it() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        for (i, &pid) in [P_OLD0, P_OLD1].iter().enumerate() {
            p.push_proc(pid, i, 0, t0 + Duration::from_millis(i as u64));
            p.set_slot(i, SLOT_IDLE);
        }

        p.begin_reload(t0, &mut sp);

        assert_eq!(p.table.generation, 1);
        assert_eq!(sp.calls.len(), 1);
        assert!(std::ptr::eq(sp.calls[0].1, p.board.slot(2)));
        assert_eq!(
            p.reload,
            Some(Reload {
                phase: ReloadPhase::Await {
                    slot: 2,
                    until: t0 + Duration::from_secs(30)
                },
                deadline: t0 + Duration::from_millis(50),
            })
        );
    }

    #[test]
    fn begin_reload_with_no_workers_finishes_immediately() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        p.begin_reload(Instant::now(), &mut sp);
        assert_eq!(p.table.generation, 1);
        assert!(p.reload.is_none());
        assert!(sp.calls.is_empty(), "no old worker needs a replacement");
    }

    #[test]
    fn reload_gate_holds_until_replacement_idle() {
        let (mut p, _sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.table.generation = 1;
        for (i, &pid) in [P_OLD0, P_OLD1, P_OLD2].iter().enumerate() {
            p.push_proc(pid, i, 0, t0 + Duration::from_millis(i as u64));
            p.set_slot(i, SLOT_IDLE);
        }
        p.push_proc(P_NEW, 3, 1, t0 + Duration::from_millis(10));
        p.set_slot(3, SLOT_STARTING);
        p.reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 3,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0,
        });

        p.reload_try_advance(t0);
        assert!(matches!(
            p.reload.unwrap().phase,
            ReloadPhase::Await { slot: 3, .. }
        ));

        p.set_slot(3, SLOT_IDLE);
        p.reload_try_advance(t0);
        assert!(matches!(
            p.reload.unwrap().phase,
            ReloadPhase::Drain {
                draining: P_OLD0,
                ..
            }
        ));
    }

    /// Under load a replacement can be ACTIVE at every probe; the gate must accept that as serving.
    #[test]
    fn reload_gate_opens_on_active_replacement() {
        let (mut p, _sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.table.generation = 1;
        p.push_proc(P_OLD0, 0, 0, t0);
        p.set_slot(0, SLOT_IDLE);
        p.push_proc(P_NEW, 3, 1, t0);
        p.set_slot(3, SLOT_ACTIVE);
        p.reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 3,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0,
        });

        p.reload_try_advance(t0);
        assert!(matches!(
            p.reload.unwrap().phase,
            ReloadPhase::Drain {
                draining: P_OLD0,
                ..
            }
        ));
    }

    #[test]
    fn reload_gate_forces_past_stuck_replacement_at_safety_cap() {
        let (mut p, _sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.table.generation = 1;
        p.push_proc(P_OLD0, 0, 0, t0);
        p.set_slot(0, SLOT_IDLE);
        p.push_proc(P_NEW, 3, 1, t0);
        p.set_slot(3, SLOT_STARTING);
        p.reload = Some(Reload {
            phase: ReloadPhase::Await { slot: 3, until: t0 },
            deadline: t0,
        });

        p.on_reload_deadline(t0 + Duration::from_millis(1));
        assert!(matches!(
            p.reload.unwrap().phase,
            ReloadPhase::Drain {
                draining: P_OLD0,
                ..
            }
        ));
    }

    #[test]
    fn reload_gate_rearms_probe_before_safety_cap() {
        let (mut p, _sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.table.generation = 1;
        p.push_proc(P_OLD0, 0, 0, t0);
        p.set_slot(0, SLOT_IDLE);
        p.push_proc(P_NEW, 3, 1, t0);
        p.set_slot(3, SLOT_STARTING);
        p.reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 3,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0,
        });

        let now = t0 + Duration::from_millis(1);
        p.on_reload_deadline(now);
        let r = p.reload.unwrap();
        assert!(matches!(r.phase, ReloadPhase::Await { slot: 3, .. }));
        assert_eq!(r.deadline, now + RELOAD_GATE_POLL);
    }

    #[test]
    fn await_gate_never_escalates() {
        let t0 = Instant::now();
        let (mut p, _sp) = test_pool(3, Scaling::Static);
        p.table.generation = 1;
        p.push_proc(P_OLD0, 0, 0, t0);
        p.set_slot(0, SLOT_IDLE);
        p.push_proc(P_NEW, 1, 1, t0);
        p.set_slot(1, SLOT_STARTING);
        p.reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 1,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0,
        });

        for i in 0..3 {
            p.on_reload_deadline(t0 + Duration::from_millis(i));
        }
        assert!(
            matches!(p.reload.unwrap().phase, ReloadPhase::Await { slot: 1, .. }),
            "the gate must stay a gate while the replacement starts"
        );
        assert!(p.table.has_pid(P_OLD0), "no old worker is drained yet");
    }

    #[test]
    fn drain_escalates_term_then_kill_against_its_pid() {
        let (mut p, _sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.table.generation = 1;
        p.push_proc(P_OLD0, 0, 0, t0);
        p.push_proc(P_NEW, 1, 1, t0);
        p.reload = Some(Reload {
            phase: ReloadPhase::Drain {
                draining: P_OLD0,
                phase: KillPhase::Quit,
            },
            deadline: t0,
        });

        let mut at = t0;
        for expected in [KillPhase::Term, KillPhase::Kill, KillPhase::Kill] {
            p.on_reload_deadline(at);
            let r = p.reload.expect("the drain chain stays armed");
            assert_eq!(
                r.phase,
                ReloadPhase::Drain {
                    draining: P_OLD0,
                    phase: expected
                }
            );
            assert_eq!(r.deadline, at + Duration::from_secs(1));
            at = r.deadline;
        }
    }

    #[test]
    fn drain_handoff_spawns_the_next_replacement_into_the_freed_slot() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        for (i, &pid) in [P_OLD0, P_OLD1].iter().enumerate() {
            p.push_proc(pid, i, 0, t0 + Duration::from_millis(i as u64));
            p.set_slot(i, SLOT_IDLE);
        }
        p.begin_reload(t0, &mut sp);
        p.set_slot(2, SLOT_IDLE);
        p.on_reload_deadline(t0 + Duration::from_millis(50));
        assert!(matches!(
            p.reload.unwrap().phase,
            ReloadPhase::Drain {
                draining: P_OLD0,
                ..
            }
        ));

        let w = p.take_proc(P_OLD0);
        let t1 = t0 + Duration::from_millis(60);
        p.on_child_exit(w, ExitVerdict::Drain, t1, false, &mut sp)
            .unwrap();

        assert_eq!(sp.calls.len(), 2);
        assert!(
            std::ptr::eq(sp.calls[1].1, p.board.slot(0)),
            "the next replacement takes the slot the drained worker freed"
        );
        assert!(matches!(
            p.reload.unwrap().phase,
            ReloadPhase::Await { slot: 0, .. }
        ));
    }

    #[test]
    fn reload_finishes_when_last_old_worker_reaped() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.table.generation = 1;
        p.push_proc(P_NEW, 0, 1, t0);
        p.reload = Some(Reload {
            phase: ReloadPhase::Drain {
                draining: P_OLD0,
                phase: KillPhase::Quit,
            },
            deadline: t0 + Duration::from_secs(30),
        });

        let drained = dead_worker(P_OLD0, 1, 0, t0);
        p.on_child_exit(drained, ExitVerdict::Drain, t0, false, &mut sp)
            .unwrap();

        assert!(p.reload.is_none());
        assert_eq!(p.table.procs.len(), 1);
        assert!(sp.calls.is_empty(), "no old worker is left to replace");
    }

    #[test]
    fn ondemand_reload_paces_one_at_a_time_without_spawning() {
        let (mut p, mut sp) = test_pool(3, Scaling::Ondemand);
        let t0 = Instant::now();
        for (i, &pid) in [P_OLD0, P_OLD1].iter().enumerate() {
            p.push_proc(pid, i, 0, t0 + Duration::from_millis(i as u64));
            p.set_slot(i, SLOT_IDLE);
        }

        p.begin_reload(t0, &mut sp);
        assert_eq!(p.table.generation, 1);
        assert!(sp.calls.is_empty(), "ondemand must not spawn a replacement");
        assert!(matches!(
            p.reload.unwrap().phase,
            ReloadPhase::Drain {
                draining: P_OLD0,
                phase: KillPhase::Quit
            }
        ));

        let w0 = p.take_proc(P_OLD0);
        p.on_child_exit(w0, ExitVerdict::Drain, t0, false, &mut sp)
            .unwrap();
        assert_eq!(p.table.procs.len(), 1);
        assert!(
            matches!(
                p.reload.unwrap().phase,
                ReloadPhase::Drain {
                    draining: P_OLD1,
                    phase: KillPhase::Quit
                }
            ),
            "the next drain starts at QUIT again"
        );

        let w1 = p.take_proc(P_OLD1);
        p.on_child_exit(w1, ExitVerdict::Drain, t0, false, &mut sp)
            .unwrap();
        assert!(p.reload.is_none());
        assert_eq!(p.table.procs.len(), 0);
    }

    #[test]
    fn unhealthy_after_ever_served_respawns_not_failboot() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.ever_served = true;
        let w = dead_worker(P_OLD0, 0, 0, t0);
        let r = p.on_child_exit(
            w,
            ExitVerdict::Unhealthy,
            t0 + Duration::from_secs(1),
            false,
            &mut sp,
        );
        assert!(r.is_ok());
        assert!(p.table.slots[0].respawn_at.is_some());
    }

    #[test]
    fn gen0_unhealthy_never_served_failboots() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        assert!(!p.ever_served);
        let w = dead_worker(P_OLD0, 0, 0, t0);
        let e = p
            .on_child_exit(w, ExitVerdict::Unhealthy, t0, false, &mut sp)
            .unwrap_err()
            .to_string();
        assert!(e.contains("http pool: worker"), "{e}");
    }

    #[test]
    fn gen1_unhealthy_never_served_respawns_not_failboot() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.table.generation = 1;
        assert!(!p.ever_served);
        let w = dead_worker(P_OLD0, 0, 1, t0);
        let r = p.on_child_exit(w, ExitVerdict::Unhealthy, t0, false, &mut sp);
        assert!(r.is_ok());
        assert!(p.table.slots[0].respawn_at.is_some());
    }

    /// A master-chosen kill respawns immediately: no failboot (that is for Unhealthy only) and no backoff.
    #[test]
    fn timeout_kill_respawns_immediately_without_failboot() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        assert!(!p.ever_served);
        let w = dead_worker(P_OLD0, 0, 0, t0);
        let r = p.on_child_exit(w, ExitVerdict::TimeoutKill, t0, false, &mut sp);
        assert!(r.is_ok());
        assert_eq!(p.table.slots[0].respawn_at, Some(t0));
        assert_eq!(p.table.slots[0].crash_streak, 0);
    }

    #[test]
    fn exit_while_stopping_schedules_nothing() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.on_child_exit(
            dead_worker(P_OLD0, 0, 0, t0),
            ExitVerdict::Crash,
            t0,
            true,
            &mut sp,
        )
        .unwrap();
        p.on_child_exit(
            dead_worker(P_OLD1, 1, 0, t0),
            ExitVerdict::Unhealthy,
            t0,
            true,
            &mut sp,
        )
        .expect("a stopping pool never failboots");

        assert_eq!(p.table.slots[0].respawn_at, None);
        assert_eq!(p.table.slots[1].respawn_at, None);
        assert!(sp.calls.is_empty());
    }

    #[test]
    fn ondemand_crash_backoff_suppresses_without_respawn() {
        let (mut p, mut sp) = test_pool(1, Scaling::Ondemand);
        let t0 = Instant::now();
        let w = dead_worker(P_OLD0, 0, 0, t0);
        p.on_child_exit(
            w,
            ExitVerdict::Crash,
            t0 + Duration::from_secs(1),
            false,
            &mut sp,
        )
        .unwrap();
        let due = p.table.slots[0]
            .respawn_at
            .expect("backoff kept as suppression");

        p.fire_due(due + Duration::from_millis(1), &mut sp);
        assert_eq!(p.table.slots[0].respawn_at, None, "suppression lifted");
        assert!(sp.calls.is_empty(), "expiry must not fork");
    }

    #[test]
    fn watchdog_marks_an_overdue_active_worker_for_timeout() {
        let (mut p, _sp) = test_pool(3, Scaling::Static);
        p.cfg.request_terminate_timeout = Duration::from_secs(2);
        let t0 = Instant::now();
        p.push_proc(P_OLD0, 0, 0, t0);
        p.set_slot(0, SLOT_ACTIVE);
        p.board
            .slot(0)
            .last_activity_ms
            .store(now_millis().saturating_sub(5_000), Relaxed);

        p.watchdog_tick();
        assert_eq!(
            p.table.procs[0].kill_intent,
            Some(KillIntent::Timeout),
            "the overdue active worker must have timeout intent"
        );
    }

    #[test]
    fn watchdog_kills_a_worker_with_timeout_intent() {
        let (mut p, _sp) = test_pool(1, Scaling::Static);
        p.cfg.request_terminate_timeout = Duration::from_secs(2);
        let mut child = TestChild::sleeper();
        p.push_proc(child.pid(), 0, 0, Instant::now());
        p.table.procs[0].kill_intent = Some(KillIntent::Timeout);
        p.set_slot(0, SLOT_ACTIVE);
        p.board
            .slot(0)
            .last_activity_ms
            .store(now_millis().saturating_sub(5_000), Relaxed);

        p.watchdog_tick();
        assert_eq!(wait_signal(&mut child), Some(libc::SIGKILL));
    }

    #[test]
    fn watchdog_spares_fresh_active_and_idle_workers() {
        let (mut p, _sp) = test_pool(3, Scaling::Static);
        p.cfg.request_terminate_timeout = Duration::from_secs(2);
        let t0 = Instant::now();
        p.push_proc(P_OLD0, 0, 0, t0);
        p.set_slot(0, SLOT_ACTIVE);
        p.board
            .slot(0)
            .last_activity_ms
            .store(now_millis(), Relaxed);
        p.push_proc(P_OLD1, 1, 0, t0);
        p.set_slot(1, SLOT_IDLE);
        p.board
            .slot(1)
            .last_activity_ms
            .store(now_millis().saturating_sub(60_000), Relaxed);

        p.watchdog_tick();
        assert!(p.table.procs.iter().all(|w| w.kill_intent.is_none()));
    }

    #[test]
    fn dynamic_ceiling_warns_once_and_never_spawns() {
        let (mut p, mut sp) = test_pool(
            2,
            Scaling::Dynamic {
                min_spare: 2,
                max_spare: 4,
            },
        );
        let t0 = Instant::now();
        for (i, &pid) in [P_OLD0, P_OLD1].iter().enumerate() {
            p.push_proc(pid, i, 0, t0);
            p.set_slot(i, SLOT_ACTIVE);
        }

        let warns = WarnCounter::default();
        tracing::subscriber::with_default(warns.clone(), || {
            p.maintenance_tick(t0, false, &mut sp);
            p.maintenance_tick(t0 + Duration::from_secs(1), false, &mut sp);
        });
        assert_eq!(warns.count(), 1, "ceiling warning must fire once");
        assert!(p.warned_max_children);
        assert!(sp.calls.is_empty(), "the ceiling ticks must not spawn");
    }

    #[test]
    fn static_refill_counts_pending_respawn_as_committed() {
        let (mut p, mut sp) = test_pool(1, Scaling::Static);
        let t0 = Instant::now();
        p.table.slots[0].schedule_immediate(t0);

        p.maintenance_tick(t0, false, &mut sp);
        assert!(
            sp.calls.is_empty(),
            "the pending respawn already covers the target"
        );
        assert_eq!(p.table.slots[0].respawn_at, Some(t0), "deadline untouched");
    }

    #[test]
    fn maintenance_is_paused_only_while_this_pool_reloads() {
        let (mut p, mut sp) = test_pool(3, Scaling::Static);
        let t0 = Instant::now();
        p.reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 0,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0 + Duration::from_secs(30),
        });

        p.maintenance_tick(t0, false, &mut sp);
        assert!(sp.calls.is_empty(), "a reloading pool must not refill");

        p.reload = None;
        p.maintenance_tick(t0, true, &mut sp);
        assert!(sp.calls.is_empty(), "a stopping master must not refill");

        p.maintenance_tick(t0, false, &mut sp);
        assert_eq!(sp.calls.len(), 3, "the pool refills once it is idle again");
    }

    /// The request bound is independent of the reload chain: an overdue worker is terminated while this pool reloads.
    #[test]
    fn watchdog_runs_while_this_pool_reloads() {
        let (mut p, mut sp) = test_pool(1, Scaling::Static);
        p.cfg.request_terminate_timeout = Duration::from_secs(2);
        let t0 = Instant::now();
        p.push_proc(P_OLD0, 0, 1, t0);
        p.set_slot(0, SLOT_ACTIVE);
        p.board
            .slot(0)
            .last_activity_ms
            .store(now_millis().saturating_sub(5_000), Relaxed);
        p.reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 1,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0 + RELOAD_GATE_POLL,
        });

        p.maintenance_tick(t0, false, &mut sp);
        assert_eq!(
            p.table.procs[0].kill_intent,
            Some(KillIntent::Timeout),
            "the overdue active worker must have timeout intent"
        );
        assert!(
            sp.calls.is_empty(),
            "scaling stays paused during the reload"
        );
    }

    #[test]
    fn ondemand_arms_only_when_a_fork_can_land() {
        let (mut p, _sp) = test_pool(1, Scaling::Ondemand);
        assert!(p.armed(false));

        let t0 = Instant::now();
        for s in &mut p.table.slots {
            s.schedule_backoff(Duration::ZERO, t0);
        }
        assert!(!p.armed(false));
    }

    #[test]
    fn ondemand_disarms_while_stopping() {
        let (p, _sp) = test_pool(1, Scaling::Ondemand);
        assert!(p.armed(false));
        assert!(!p.armed(true));
    }

    #[test]
    fn ondemand_disarms_while_this_pool_reloads() {
        let (mut p, _sp) = test_pool(1, Scaling::Ondemand);
        assert!(p.armed(false));
        p.reload = Some(Reload {
            phase: ReloadPhase::Drain {
                draining: P_OLD0,
                phase: KillPhase::Quit,
            },
            deadline: Instant::now(),
        });
        assert!(!p.armed(false));
    }

    /// Static and dynamic workers accept in the children; the master watches only its self-pipe.
    #[test]
    fn non_ondemand_never_arms_listeners() {
        let (p, _sp) = test_pool(1, Scaling::Static);
        assert!(!p.armed(false));
    }

    #[test]
    fn next_deadline_is_the_earliest_of_reload_and_respawns() {
        let (mut p, _sp) = test_pool(3, Scaling::Static);
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
