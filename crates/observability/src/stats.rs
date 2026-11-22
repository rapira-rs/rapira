use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use rapira_scoreboard::{Pool, SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SLOT_STARTING};

/// A worker state and its `state` label value.
pub(crate) struct StateLabel {
    pub state: u32,
    pub label: &'static str,
}

/// Worker states in output order.
pub(crate) const STATES: [StateLabel; 4] = [
    StateLabel {
        state: SLOT_STARTING,
        label: "starting",
    },
    StateLabel {
        state: SLOT_IDLE,
        label: "idle",
    },
    StateLabel {
        state: SLOT_ACTIVE,
        label: "active",
    },
    StateLabel {
        state: SLOT_DRAINING,
        label: "draining",
    },
];

/// The values of one pool at one scrape.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PoolStats {
    pub name: &'static str,
    pub configured: usize,
    /// Worker counts in `STATES` order.
    pub states: [u64; 4],
    pub requests: u64,
    pub failed: u64,
    pub failed_on_full_queue: u64,
    pub queued: u64,
    pub script_restarts: u64,
}

pub(crate) fn board_stats(board: &[Arc<Pool>]) -> Vec<PoolStats> {
    board.iter().map(|pool| pool_stats(pool)).collect()
}

fn pool_stats(pool: &Pool) -> PoolStats {
    let refused = pool.refused.load(Relaxed);
    let mut stats = PoolStats {
        name: pool.name,
        configured: pool.workers.len(),
        requests: refused,
        failed: refused,
        queued: pool.pending.load(Relaxed),
        failed_on_full_queue: pool.failed_on_full_queue.load(Relaxed),
        ..Default::default()
    };
    for worker in &pool.workers {
        let state = worker.state.load(Relaxed);
        if let Some(i) = STATES.iter().position(|known| known.state == state) {
            stats.states[i] += 1;
        }
        let (handled, failed) = worker.totals();
        stats.requests += handled;
        stats.failed += failed;
        stats.script_restarts += worker.recycles.load(Relaxed);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_stats_sums_each_pool() {
        struct Case {
            name: &'static str,
            // State, handled units, errors and recycles for each worker.
            workers: &'static [(u32, u64, u64, u64)],
            refused: u64,
            failed_on_full_queue: u64,
            pending: u64,
            want: PoolStats,
        }
        let cases = [
            Case {
                name: "host refusals count once in both totals",
                workers: &[(SLOT_ACTIVE, 7, 2, 1), (SLOT_IDLE, 4, 1, 2)],
                refused: 5,
                failed_on_full_queue: 3,
                pending: 6,
                want: PoolStats {
                    name: "http",
                    configured: 2,
                    states: [0, 1, 1, 0],
                    requests: 16,
                    failed: 8,
                    failed_on_full_queue: 3,
                    queued: 6,
                    script_restarts: 3,
                },
            },
            Case {
                name: "a second pool has its own queue and worker counts",
                workers: &[
                    (SLOT_STARTING, 0, 0, 0),
                    (SLOT_DRAINING, 3, 1, 4),
                    (SLOT_DRAINING, 0, 0, 0),
                    (SLOT_ACTIVE, 9, 0, 2),
                ],
                refused: 2,
                failed_on_full_queue: 0,
                pending: 1,
                want: PoolStats {
                    name: "grpc",
                    configured: 4,
                    states: [1, 0, 1, 2],
                    requests: 14,
                    failed: 3,
                    failed_on_full_queue: 0,
                    queued: 1,
                    script_restarts: 6,
                },
            },
        ];
        let board: Vec<_> = cases
            .iter()
            .map(|case| {
                let pool = Pool::new(case.want.name, case.workers.len());
                pool.refused.store(case.refused, Relaxed);
                pool.failed_on_full_queue
                    .store(case.failed_on_full_queue, Relaxed);
                pool.pending.store(case.pending, Relaxed);
                for (worker, &(state, handled, errors, recycles)) in
                    pool.workers.iter().zip(case.workers)
                {
                    worker.state.store(state, Relaxed);
                    for i in 0..handled {
                        worker.record_outcome(i < errors);
                    }
                    worker.recycles.store(recycles, Relaxed);
                }
                Arc::new(pool)
            })
            .collect();
        let stats = board_stats(&board);
        assert_eq!(stats.len(), cases.len());
        for (stats, case) in stats.into_iter().zip(cases) {
            assert_eq!(stats, case.want, "{}", case.name);
        }
    }
}
