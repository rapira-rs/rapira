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
    /// Filled after the board read. None: not read yet, or the read failed.
    pub memory: Option<Memory>,
}

/// The values of one pool at one scrape.
#[derive(Debug, PartialEq)]
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

/// The stats of every pool except `own`, the pool of the observability process.
pub(crate) fn board_stats(
    board: &Scoreboard,
    regions: &[PoolRegion],
    own: usize,
) -> Vec<PoolStats> {
    regions
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != own)
        .map(|(_, region)| pool_stats(board, region))
        .collect()
}

/// Counters and the queue sum over all slots of the pool: a slot keeps the counts of every worker that it held, and `clear` empties the queue of a dead worker. A state value that is not known is not counted.
fn pool_stats(board: &Scoreboard, region: &PoolRegion) -> PoolStats {
    let mut stats = PoolStats {
        name: region.name,
        configured: region.processes,
        states: [0; 4],
        requests: 0,
        failed: 0,
        failed_on_full_queue: 0,
        queued: 0,
        script_restarts: 0,
        exits: [0; 5],
        workers: Vec::new(),
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
                memory: None,
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

    /// Slots 0 and 1 belong to the observability pool, slots 2 to 5 to an http pool of 2 workers, and slots 6 to 9 to a grpc pool of 2 workers. Each pair of states has different counts in at least one pool, and each grpc exit reason has its own count, so a swap of two states or two exit reasons fails the test.
    #[test]
    fn board_stats_sums_each_pool_except_its_own() {
        let board = Scoreboard::create(10).unwrap();
        let slot = |i: usize| board.slot(i);
        let place = |i: usize, state: u32, pid: u32| {
            slot(i).state.store(state, Relaxed);
            slot(i).pid.store(pid, Relaxed);
        };
        // The observability pool: left out of the output.
        place(0, SLOT_IDLE, 100);
        slot(0).handled.store(99, Relaxed);
        // http slot 0: an active worker.
        place(2, SLOT_ACTIVE, 201);
        slot(2).handled.store(10, Relaxed);
        slot(2).errors.store(1, Relaxed);
        slot(2).pending.store(2, Relaxed);
        slot(2).recycles.store(1, Relaxed);
        slot(2).exits_crashed.store(1, Relaxed);
        slot(2).failed_on_full_queue.store(4, Relaxed);
        // http slot 1: an idle worker.
        place(3, SLOT_IDLE, 202);
        slot(3).handled.store(5, Relaxed);
        slot(3).exits_recycled.store(2, Relaxed);
        // http slot 2: free. Its counts stay in the totals.
        place(4, SLOT_FREE, 0);
        slot(4).handled.store(7, Relaxed);
        slot(4).errors.store(2, Relaxed);
        slot(4).exits_drained.store(1, Relaxed);
        slot(4).failed_on_full_queue.store(5, Relaxed);
        // http slot 3: a state value that is not known. No state count, but a live pid.
        place(5, 9, 204);
        slot(5).handled.store(1, Relaxed);
        // grpc slots 0 to 3: one starting, two draining and one active worker.
        place(6, SLOT_STARTING, 301);
        slot(6).exits_drained.store(1, Relaxed);
        slot(6).exits_recycled.store(2, Relaxed);
        place(7, SLOT_DRAINING, 302);
        slot(7).exits_unhealthy.store(3, Relaxed);
        place(8, SLOT_DRAINING, 303);
        slot(8).exits_timeout.store(4, Relaxed);
        place(9, SLOT_ACTIVE, 304);
        slot(9).exits_crashed.store(5, Relaxed);
        slot(9).failed_on_full_queue.store(6, Relaxed);
        let regions = [
            PoolRegion {
                name: "observability",
                processes: 1,
                slots: 0..2,
            },
            PoolRegion {
                name: "http",
                processes: 2,
                slots: 2..6,
            },
            PoolRegion {
                name: "grpc",
                processes: 2,
                slots: 6..10,
            },
        ];

        let got = board_stats(&board, &regions, 0);

        assert_eq!(
            got,
            vec![
                PoolStats {
                    name: "http",
                    configured: 2,
                    states: [0, 1, 1, 0],
                    requests: 23,
                    failed: 3,
                    failed_on_full_queue: 9,
                    queued: 2,
                    script_restarts: 1,
                    exits: [1, 2, 0, 0, 1],
                    workers: vec![
                        Worker {
                            index: 0,
                            pid: 201,
                            memory: None
                        },
                        Worker {
                            index: 1,
                            pid: 202,
                            memory: None
                        },
                        Worker {
                            index: 3,
                            pid: 204,
                            memory: None
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
                            memory: None
                        },
                        Worker {
                            index: 1,
                            pid: 302,
                            memory: None
                        },
                        Worker {
                            index: 2,
                            pid: 303,
                            memory: None
                        },
                        Worker {
                            index: 3,
                            pid: 304,
                            memory: None
                        },
                    ],
                },
            ]
        );
    }
}
