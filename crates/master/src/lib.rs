use std::os::fd::{OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use rapira_scoreboard::{SB_MAX_SLOTS, Scoreboard, SharedSlot};

mod events;
mod lifeline;
mod pctl;
mod pidfile;
mod pool;
mod process;
mod scaling;
mod signals;

pub use lifeline::{Lifeline, spawn_lifeline_watch};
pub use signals::block_early_signals;

/// Worker exit-code protocol: the worker emits, the master consumes; any other code is a crash.
pub const WORKER_EXIT_DRAINED: i32 = 0;
/// Quota recycle (e.g. max_requests): immediate respawn, no backoff.
pub const WORKER_EXIT_RECYCLE: i32 = 88;
/// Self-reported unhealthy: respawn with backoff; gen-0 with zero handled requests is a boot failure.
pub const WORKER_EXIT_UNHEALTHY: i32 = 89;
/// Exit code the caller uses when [`run`] returns a boot-failure error.
pub const MASTER_EXIT_FAILBOOT: i32 = 70;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    Static,
    Dynamic { min_spare: usize, max_spare: usize },
    Ondemand,
}

/// One plugin's worker set. The master supervises every pool independently.
pub struct PoolConfig {
    /// Config table the pool came from ("http"). Log lines carry it as `{name} pool:`; messages that quote a key use `{name}.pool.<key>`.
    pub name: &'static str,
    /// Static worker count, or the max-children ceiling under dynamic/ondemand.
    pub processes: usize,
    pub scaling: Scaling,
    /// Ondemand only: idle worker lifetime before a QUIT.
    pub process_idle_timeout: Duration,
    /// Wall-clock bound on one request: the worker is TERM-killed, then KILLed, and replaced. Zero disables.
    pub request_terminate_timeout: Duration,
    /// This pool's bound listener fds, polled only under `Ondemand`; the master never accepts on them.
    pub listeners: Vec<RawFd>,
}

impl PoolConfig {
    /// Two slots per worker so a reload replacement fits next to the worker it replaces.
    pub fn slots(&self) -> usize {
        self.processes * 2
    }
}

pub struct MasterConfig {
    pub pools: Vec<PoolConfig>,
    /// Stop/reload QUIT to TERM escalation grace.
    pub process_control_timeout: Duration,
    pub pidfile: Option<PathBuf>,
}

impl MasterConfig {
    /// Two slots per worker, pools contiguous in order. Names the pool that pushes the total past the cap.
    /// Every `processes` is at least 1; the config layer enforces the floor.
    pub fn scoreboard_slots(&self) -> anyhow::Result<usize> {
        let mut slots: usize = 0;
        for p in &self.pools {
            slots = slots.saturating_add(p.slots());
            anyhow::ensure!(
                slots <= SB_MAX_SLOTS,
                "{}.pool.processes ({}) raises the worker total to {}, above the supported maximum ({})",
                p.name,
                p.processes,
                slots / 2,
                SB_MAX_SLOTS / 2
            );
        }
        Ok(slots)
    }
}

/// Handed to the worker closure in the child, after post-fork hygiene.
pub struct WorkerEnv {
    /// Index of the pool this worker belongs to, into `MasterConfig::pools`.
    pub pool: usize,
    /// Read end of the master lifeline: EOF means the master died, so drain.
    pub lifeline: OwnedFd,
    pub slot_view: &'static SharedSlot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// All workers drained cleanly: the caller tears down PHP and exits 0.
    Drained,
    /// Second stop signal or TERM while stopping: the caller exits 130 without PHP teardown.
    Forced,
}

/// An auxiliary process supervised by the master's normal event loop.
pub trait Service {
    fn tick(&mut self);
    fn on_exit(&mut self, pid: libc::pid_t, status: libc::c_int) -> bool;
}

impl Service for () {
    fn tick(&mut self) {}
    fn on_exit(&mut self, _pid: libc::pid_t, _status: libc::c_int) -> bool {
        false
    }
}

/// Returns in the parent on a clean or forced stop; in a forked child it never returns: the worker closure runs and the child `_exit`s.
/// `scoreboard` must have `cfg.scoreboard_slots()` slots: `Master::new` slices it with the same arithmetic and panics on a smaller board.
pub fn run(
    cfg: MasterConfig,
    scoreboard: Scoreboard,
    worker: impl FnMut(WorkerEnv) -> i32,
) -> anyhow::Result<StopReason> {
    run_with_service(cfg, scoreboard, worker, Box::new(()))
}

pub fn run_with_service(
    cfg: MasterConfig,
    scoreboard: Scoreboard,
    worker: impl FnMut(WorkerEnv) -> i32,
    service: Box<dyn Service>,
) -> anyhow::Result<StopReason> {
    let self_pipe: signals::SelfPipe = signals::install_master_signals()?;
    let lifeline: Lifeline = Lifeline::create()?;
    let _pidfile: Option<pidfile::PidFile> = match &cfg.pidfile {
        Some(p) => Some(pidfile::PidFile::write(p)?),
        None => None,
    };

    let forker = process::Forker {
        self_pipe,
        lifeline,
        worker,
    };
    let mut master = events::Master::new(cfg, scoreboard, forker);
    master.service = service;
    master.run_loop()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(name: &'static str, processes: usize) -> PoolConfig {
        PoolConfig {
            name,
            processes,
            scaling: Scaling::Static,
            process_idle_timeout: Duration::from_secs(10),
            request_terminate_timeout: Duration::ZERO,
            listeners: Vec::new(),
        }
    }

    fn cfg(pools: Vec<PoolConfig>) -> MasterConfig {
        MasterConfig {
            pools,
            process_control_timeout: Duration::from_secs(30),
            pidfile: None,
        }
    }

    #[test]
    fn scoreboard_slots_sums_two_per_worker_over_pools() {
        assert_eq!(
            cfg(vec![pool("http", 3), pool("grpc", 2)])
                .scoreboard_slots()
                .unwrap(),
            10
        );
        assert_eq!(
            cfg(vec![pool("http", SB_MAX_SLOTS / 2)])
                .scoreboard_slots()
                .unwrap(),
            SB_MAX_SLOTS
        );
    }

    #[test]
    fn scoreboard_slots_names_the_pool_that_crosses_the_cap() {
        let e = cfg(vec![pool("http", SB_MAX_SLOTS / 2), pool("grpc", 1)])
            .scoreboard_slots()
            .unwrap_err()
            .to_string();
        assert!(e.contains("grpc.pool.processes (1)"), "{e}");
        assert!(!e.contains("http.pool"), "{e}");
        assert!(
            e.contains(&format!(
                "raises the worker total to {}, above the supported maximum ({})",
                SB_MAX_SLOTS / 2 + 1,
                SB_MAX_SLOTS / 2
            )),
            "{e}"
        );
    }
}
