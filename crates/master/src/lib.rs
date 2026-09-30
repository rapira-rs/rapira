use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

use rapira_scoreboard::{PoolRegion, SB_MAX_SLOTS, Scoreboard, SharedSlot};

mod events;
mod lifeline;
mod pctl;
mod pidfile;
mod pool;
mod process;
mod signals;

pub use lifeline::spawn_lifeline_watch;
pub use signals::{block_early_signals, wait_signal};

/// Worker exit-code protocol: the worker emits, the master consumes; any other code is a crash.
pub const WORKER_EXIT_DRAINED: i32 = 0;
/// Quota recycle (e.g. max_requests): immediate respawn, no backoff.
pub const WORKER_EXIT_RECYCLE: i32 = 88;
/// Self-reported unhealthy: respawn with backoff; gen-0 with zero handled requests is a boot failure.
pub const WORKER_EXIT_UNHEALTHY: i32 = 89;
/// Exit code the caller uses when [`run`] returns a boot-failure error.
pub const MASTER_EXIT_FAILBOOT: i32 = 70;

/// One plugin's worker set. The master supervises every pool independently.
pub struct PoolConfig {
    /// Config table the pool came from ("http"). Log lines carry it as `{name} pool:`; messages that quote a key use `{name}.pool.<key>`.
    pub name: &'static str,
    /// Worker count. The master keeps this many workers running.
    pub processes: usize,
    /// Wall-clock bound on one request: the worker is TERM-killed, then KILLed, and replaced. Zero disables.
    pub request_terminate_timeout: Duration,
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
    /// Two slots per worker, pools contiguous in `pools` order. Names the pool that pushes the total past the cap.
    fn regions(&self) -> anyhow::Result<Vec<PoolRegion>> {
        let mut base: usize = 0;
        self.pools
            .iter()
            .map(|p| {
                let end = base.saturating_add(p.slots());
                anyhow::ensure!(
                    end <= SB_MAX_SLOTS,
                    "{}.pool.processes ({}) raises the worker total to {}, above the supported maximum ({})",
                    p.name,
                    p.processes,
                    end / 2,
                    SB_MAX_SLOTS / 2
                );
                let slots = base..end;
                base = end;
                Ok(PoolRegion {
                    name: p.name,
                    processes: p.processes,
                    slots,
                })
            })
            .collect()
    }
}

/// Handed to the worker closure in the child, after post-fork hygiene.
pub struct WorkerEnv {
    /// Index of the pool this worker belongs to, into `MasterConfig::pools`.
    pub pool: usize,
    /// Read end of the master lifeline: EOF means the master died, so drain.
    pub lifeline: OwnedFd,
    pub slot_view: &'static SharedSlot,
    /// The whole board: the slots of every pool.
    pub board: Scoreboard,
    /// The part of `board` that each pool owns, in `MasterConfig::pools` order.
    pub regions: &'static [PoolRegion],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// All workers drained cleanly: the caller tears down PHP and exits 0.
    Drained,
    /// TERM or INT while stopping: the caller exits 130 without PHP teardown.
    Forced,
}

/// Returns in the parent on a clean or forced stop; in a forked child it never returns: the worker closure runs and the child `_exit`s.
pub fn run(cfg: MasterConfig, worker: impl FnMut(WorkerEnv) -> i32) -> anyhow::Result<StopReason> {
    let regions = cfg.regions()?;
    let scoreboard: Scoreboard = Scoreboard::create(regions.last().map_or(0, |r| r.slots.end))?;
    // Built once per boot and never freed, as the board: every forked child reads the same regions.
    let regions: &'static [PoolRegion] = regions.leak();
    let self_pipe: signals::SelfPipe = signals::install_master_signals()?;
    let lifeline: lifeline::Lifeline = lifeline::Lifeline::create()?;
    let _pidfile: Option<pidfile::PidFile> = match &cfg.pidfile {
        Some(p) => Some(pidfile::PidFile::write(p)?),
        None => None,
    };

    let forker = process::Forker {
        self_pipe,
        lifeline,
        board: scoreboard,
        regions,
        worker: Box::new(worker),
    };
    let mut master = events::Master::new(cfg, forker);
    master.run_loop()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(name: &'static str, processes: usize) -> PoolConfig {
        PoolConfig {
            name,
            processes,
            request_terminate_timeout: Duration::ZERO,
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
    fn regions_name_the_pool_that_crosses_the_cap() {
        let e = cfg(vec![pool("http", SB_MAX_SLOTS / 2), pool("grpc", 1)])
            .regions()
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

    #[test]
    fn regions_place_the_pools_on_the_board() {
        struct Case {
            name: &'static str,
            pools: Vec<PoolConfig>,
            want: Vec<PoolRegion>,
        }
        let cases = [
            Case {
                name: "pool order with two slots per worker",
                pools: vec![pool("observability", 1), pool("http", 3), pool("grpc", 2)],
                want: vec![
                    PoolRegion {
                        name: "observability",
                        processes: 1,
                        slots: 0..2,
                    },
                    PoolRegion {
                        name: "http",
                        processes: 3,
                        slots: 2..8,
                    },
                    PoolRegion {
                        name: "grpc",
                        processes: 2,
                        slots: 8..12,
                    },
                ],
            },
            Case {
                name: "one pool fills the board up to the cap",
                pools: vec![pool("http", SB_MAX_SLOTS / 2)],
                want: vec![PoolRegion {
                    name: "http",
                    processes: SB_MAX_SLOTS / 2,
                    slots: 0..SB_MAX_SLOTS,
                }],
            },
        ];
        for case in cases {
            let regions = cfg(case.pools).regions().unwrap();
            assert_eq!(regions, case.want, "{}", case.name);
        }
    }
}
