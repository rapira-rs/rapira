use std::sync::atomic::Ordering::Relaxed;

use rapira_scoreboard::{
    PoolRegion, SLOT_ACTIVE, SLOT_DRAINING, SLOT_FREE, SLOT_IDLE, SLOT_STARTING, Scoreboard,
};

use crate::memory::Memory;

/// Slot states in output order, with their label values.
pub(crate) const STATES: [(u32, &str); 4] = [
    (SLOT_STARTING, "starting"),
    (SLOT_IDLE, "idle"),
    (SLOT_ACTIVE, "active"),
    (SLOT_DRAINING, "draining"),
];

/// Exit reasons in output order. `PoolStats::exits` holds the counts in this order.
pub(crate) const EXIT_REASONS: [&str; 5] =
    ["drained", "recycled", "unhealthy", "timeout", "crashed"];

/// A live worker: a slot with a pid other than 0 and a state other than FREE.
#[derive(Debug, PartialEq)]
pub(crate) struct Worker {
    /// The slot index in the pool.
    pub index: usize,
    pub pid: u32,
    /// Filled after the board read. Both values are None until the read, or when the read fails.
    pub memory: Memory,
}

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
    /// Exit counts in `EXIT_REASONS` order.
    pub exits: [u64; 5],
    pub workers: Vec<Worker>,
}

/// The stats of each pool in `pools`.
pub(crate) fn board_stats(board: &Scoreboard, pools: &[PoolRegion]) -> Vec<PoolStats> {
    pools.iter().map(|pool| pool_stats(board, pool)).collect()
}

/// Counters and the queue sum over all slots of the pool: a slot keeps the counts of every worker that it held, and `clear` empties the queue of a dead worker.
fn pool_stats(board: &Scoreboard, region: &PoolRegion) -> PoolStats {
    let mut stats = PoolStats {
        name: region.name,
        configured: region.processes,
        ..Default::default()
    };
    for (index, s) in board.slots()[region.slots.clone()].iter().enumerate() {
        let state = s.state.load(Relaxed);
        if let Some(i) = STATES.iter().position(|&(known, _)| known == state) {
            stats.states[i] += 1;
        }
        let pid = s.pid.load(Relaxed);
        if pid != 0 && state != SLOT_FREE {
            stats.workers.push(Worker {
                index,
                pid,
                memory: Memory::default(),
            });
        }
        stats.requests += s.handled.load(Relaxed);
        stats.failed += s.errors.load(Relaxed);
        stats.failed_on_full_queue += s.failed_on_full_queue.load(Relaxed);
        stats.queued += s.pending.load(Relaxed);
        stats.script_restarts += s.recycles.load(Relaxed);
        let exits = [
            &s.exits_drained,
            &s.exits_recycled,
            &s.exits_unhealthy,
            &s.exits_timeout,
            &s.exits_crashed,
        ];
        for (total, counter) in stats.exits.iter_mut().zip(exits) {
            *total += counter.load(Relaxed);
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slots 0 to 3 belong to an http pool of 2 workers, and slots 4 to 7 to a grpc pool of 2 workers. Each pair of states has different counts in at least one pool, and each grpc exit reason has its own count, so a swap of two states or two exit reasons fails the test.
    #[test]
    fn board_stats_sums_each_pool() {
        let board = Scoreboard::create(8).unwrap();
        let slot = |i: usize| board.slot(i);
        let place = |i: usize, state: u32, pid: u32| {
            slot(i).state.store(state, Relaxed);
            slot(i).pid.store(pid, Relaxed);
        };
        // http slot 0: an active worker.
        place(0, SLOT_ACTIVE, 201);
        slot(0).handled.store(10, Relaxed);
        slot(0).errors.store(1, Relaxed);
        slot(0).pending.store(2, Relaxed);
        slot(0).recycles.store(1, Relaxed);
        slot(0).exits_crashed.store(1, Relaxed);
        slot(0).failed_on_full_queue.store(4, Relaxed);
        // http slot 1: an idle worker.
        place(1, SLOT_IDLE, 202);
        slot(1).handled.store(5, Relaxed);
        slot(1).exits_recycled.store(2, Relaxed);
        // http slot 2: free. Its counts stay in the totals.
        place(2, SLOT_FREE, 0);
        slot(2).handled.store(7, Relaxed);
        slot(2).errors.store(2, Relaxed);
        slot(2).exits_drained.store(1, Relaxed);
        slot(2).failed_on_full_queue.store(5, Relaxed);
        // grpc slots 0 to 3: one starting, two draining and one active worker.
        place(4, SLOT_STARTING, 301);
        slot(4).exits_drained.store(1, Relaxed);
        slot(4).exits_recycled.store(2, Relaxed);
        place(5, SLOT_DRAINING, 302);
        slot(5).exits_unhealthy.store(3, Relaxed);
        place(6, SLOT_DRAINING, 303);
        slot(6).exits_timeout.store(4, Relaxed);
        place(7, SLOT_ACTIVE, 304);
        slot(7).exits_crashed.store(5, Relaxed);
        slot(7).failed_on_full_queue.store(6, Relaxed);
        let pools = [
            PoolRegion {
                name: "http",
                processes: 2,
                slots: 0..4,
            },
            PoolRegion {
                name: "grpc",
                processes: 2,
                slots: 4..8,
            },
        ];

        let got = board_stats(&board, &pools);

        assert_eq!(
            got,
            vec![
                PoolStats {
                    name: "http",
                    configured: 2,
                    states: [0, 1, 1, 0],
                    requests: 22,
                    failed: 3,
                    failed_on_full_queue: 9,
                    queued: 2,
                    script_restarts: 1,
                    exits: [1, 2, 0, 0, 1],
                    workers: vec![
                        Worker {
                            index: 0,
                            pid: 201,
                            memory: Memory::default()
                        },
                        Worker {
                            index: 1,
                            pid: 202,
                            memory: Memory::default()
                        },
                    ],
                },
                PoolStats {
                    name: "grpc",
                    configured: 2,
                    states: [1, 0, 1, 2],
                    requests: 0,
                    failed: 0,
                    failed_on_full_queue: 6,
                    queued: 0,
                    script_restarts: 0,
                    exits: [1, 2, 3, 4, 5],
                    workers: vec![
                        Worker {
                            index: 0,
                            pid: 301,
                            memory: Memory::default()
                        },
                        Worker {
                            index: 1,
                            pid: 302,
                            memory: Memory::default()
                        },
                        Worker {
                            index: 2,
                            pid: 303,
                            memory: Memory::default()
                        },
                        Worker {
                            index: 3,
                            pid: 304,
                            memory: Memory::default()
                        },
                    ],
                },
            ]
        );
    }
}
