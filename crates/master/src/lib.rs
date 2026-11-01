//! The pre-fork supervisor: the worker contract, the config and [`run`], which forks the pools and supervises them until a stop.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use rapira_scoreboard::{PoolRegion, SB_MAX_SLOTS, SharedSlot};

mod events;
mod lifeline;
mod pctl;
mod pidfile;
mod pool;
mod process;
mod signals;

pub use lifeline::spawn_lifeline_watch;
pub use signals::{block_early_signals, wait_signal};

/// Exit 0: the worker drained. Outside a stop the master forks a replacement at once, without backoff. The worker emits these codes and the master consumes them; any other code is a crash.
pub const WORKER_EXIT_DRAINED: i32 = 0;
/// Self-reported unhealthy: respawn with backoff. While the pool has no serving worker, no handled request and no recycle, it is a boot failure.
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
    /// RSS of one worker in MiB: above it the worker is QUIT-drained and replaced. Zero disables.
    pub max_memory_mb: u64,
}

/// The pools of one boot. [`run`] forks their workers and supervises them.
pub struct MasterConfig {
    /// One entry per pool, in the order that `WorkerEnv::pool` indexes.
    pub pools: Vec<PoolConfig>,
    /// Stop QUIT to TERM escalation grace.
    pub process_control_timeout: Duration,
    /// Written at start, removed when [`run`] returns.
    pub pidfile: Option<PathBuf>,
}

impl MasterConfig {
    /// One slot per worker; pools contiguous in `pools` order. Names the pool that pushes the total past the cap.
    fn regions(&self) -> anyhow::Result<Vec<PoolRegion>> {
        let mut regions: Vec<PoolRegion> = Vec::with_capacity(self.pools.len());
        let mut base: usize = 0;
        for p in &self.pools {
            let end = base + p.processes;
            anyhow::ensure!(
                end <= SB_MAX_SLOTS,
                "{}.pool.processes ({}) raises the worker total to {}, above the supported maximum ({})",
                p.name,
                p.processes,
                end,
                SB_MAX_SLOTS
            );
            regions.push(PoolRegion {
                name: p.name,
                slots: base..end,
            });
            base = end;
        }
        Ok(regions)
    }
}

/// Handed to the worker closure in the child, after post-fork hygiene.
pub struct WorkerEnv {
    /// Index of the pool this worker belongs to, into `MasterConfig::pools`.
    pub pool: usize,
    /// Read end of the master lifeline: EOF means the master died, so drain.
    pub lifeline: std::io::PipeReader,
    /// The slot of this worker on the board.
    pub slot_view: &'static SharedSlot,
    /// The whole board: the slots of every pool.
    pub board: &'static [SharedSlot],
    /// The part of `board` that each pool owns, in `MasterConfig::pools` order.
    pub regions: &'static [PoolRegion],
}

/// Why [`run`] returned; the caller picks the exit status from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// No worker is left: each one drained or the stop escalation ended it. The caller tears down PHP and exits 0.
    Drained,
    /// TERM or INT while stopping: the caller exits 130 without PHP teardown.
    Forced,
}

/// Returns in the parent on a clean or forced stop, or with an error when `poll` fails or a worker of a pool that never served exits unhealthy. In a forked child it never returns: the worker closure runs and the child `_exit`s.
pub fn run(
    cfg: MasterConfig,
    worker: impl FnMut(WorkerEnv) -> i32 + 'static,
) -> anyhow::Result<StopReason> {
    let regions = cfg.regions()?;
    let board = rapira_scoreboard::create(regions.last().map_or(0, |r| r.slots.end))
        .context("scoreboard mmap failed")?;
    // Built once per boot and never freed, as the board: every forked child reads the same regions.
    let regions: &'static [PoolRegion] = regions.leak();
    let self_pipe: signals::SelfPipe = signals::install_master_signals()?;
    let (lifeline_rd, lifeline_wr) = std::io::pipe().context("lifeline pipe")?;
    // A named binding keeps the pidfile until run returns. With let _, Drop removes the file at once.
    let _pidfile: Option<pidfile::PidFile> = match &cfg.pidfile {
        Some(p) => {
            Some(pidfile::PidFile::write(p).with_context(|| format!("pidfile {}", p.display()))?)
        }
        None => None,
    };

    let forker = process::Forker {
        self_pipe,
        lifeline_rd,
        lifeline_wr,
        board,
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
            max_memory_mb: 0,
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
        let e = cfg(vec![pool("http", SB_MAX_SLOTS), pool("grpc", 1)])
            .regions()
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            format!(
                "grpc.pool.processes (1) raises the worker total to {}, above the supported maximum ({})",
                SB_MAX_SLOTS + 1,
                SB_MAX_SLOTS
            )
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
                name: "pool order with one slot per worker",
                pools: vec![pool("observability", 1), pool("http", 3), pool("grpc", 2)],
                want: vec![
                    PoolRegion {
                        name: "observability",
                        slots: 0..1,
                    },
                    PoolRegion {
                        name: "http",
                        slots: 1..4,
                    },
                    PoolRegion {
                        name: "grpc",
                        slots: 4..6,
                    },
                ],
            },
            Case {
                name: "one pool fills the board up to the cap",
                pools: vec![pool("http", SB_MAX_SLOTS)],
                want: vec![PoolRegion {
                    name: "http",
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
