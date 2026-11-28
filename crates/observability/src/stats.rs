use rapira_sapi::pool::PoolSnapshot;

/// The values of one pool at one scrape.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PoolStats {
    pub name: &'static str,
    pub configured: usize,
    /// Worker counts in `State::ALL` order.
    pub states: [u64; 4],
    pub requests: u64,
    pub failed: u64,
    pub failed_on_full_queue: u64,
    pub queued: u64,
    pub script_restarts: u64,
}

pub(crate) fn pool_stats(pool: &PoolSnapshot) -> PoolStats {
    let mut stats = PoolStats {
        name: pool.name,
        configured: pool.workers.len(),
        requests: pool.refused,
        failed: pool.refused,
        queued: pool.pending,
        failed_on_full_queue: pool.failed_on_full_queue,
        ..Default::default()
    };
    for worker in &pool.workers {
        stats.states[worker.state as usize] += 1;
        stats.requests += worker.handled;
        stats.failed += worker.errors;
        stats.script_restarts += worker.recycles;
    }
    stats
}

#[cfg(test)]
mod tests {
    use rapira_sapi::pool::{State, WorkerSnapshot};

    use super::*;

    fn worker(state: State, handled: u64, errors: u64, recycles: u64) -> WorkerSnapshot {
        WorkerSnapshot {
            state,
            handled,
            errors,
            recycles,
            last_activity_ms: 0,
        }
    }

    #[test]
    fn pool_stats_sums_the_workers_of_a_pool() {
        struct Case {
            name: &'static str,
            pool: PoolSnapshot,
            want: PoolStats,
        }
        let cases = [
            Case {
                name: "host refusals count once in both totals",
                pool: PoolSnapshot {
                    name: "http",
                    pending: 6,
                    failed_on_full_queue: 3,
                    refused: 5,
                    workers: vec![worker(State::Active, 7, 2, 1), worker(State::Idle, 4, 1, 2)],
                },
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
                pool: PoolSnapshot {
                    name: "grpc",
                    pending: 1,
                    failed_on_full_queue: 0,
                    refused: 2,
                    workers: vec![
                        worker(State::Starting, 0, 0, 0),
                        worker(State::Draining, 3, 1, 4),
                        worker(State::Draining, 0, 0, 0),
                        worker(State::Active, 9, 0, 2),
                    ],
                },
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
        for case in cases {
            assert_eq!(pool_stats(&case.pool), case.want, "{}", case.name);
        }
    }
}
