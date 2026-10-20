use rapira_scoreboard::{PoolRegion, SharedSlot};

/// The pools in `pools` without a worker that can serve: no slot of the pool is idle or active.
pub(crate) fn unready(board: &[SharedSlot], pools: &[PoolRegion]) -> Vec<&'static str> {
    pools
        .iter()
        .filter(|pool| !board[pool.slots.clone()].iter().any(SharedSlot::serving))
        .map(|pool| pool.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;

    use rapira_scoreboard::{SLOT_ACTIVE, SLOT_DRAINING, SLOT_FREE, SLOT_IDLE, SLOT_STARTING};

    use super::*;

    const HTTP: PoolRegion = PoolRegion {
        name: "http",
        processes: 2,
        slots: 0..4,
    };
    const GRPC: PoolRegion = PoolRegion {
        name: "grpc",
        processes: 2,
        slots: 4..8,
    };

    #[test]
    fn unready_lists_each_pool_without_a_serving_worker() {
        struct Slot {
            index: usize,
            state: u32,
            handled: u64,
        }
        struct Case {
            name: &'static str,
            pools: Vec<PoolRegion>,
            /// The slots that the case sets. The other slots stay FREE with no counts.
            slots: &'static [Slot],
            want: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "an idle worker",
                pools: vec![HTTP],
                slots: &[Slot {
                    index: 0,
                    state: SLOT_IDLE,
                    handled: 0,
                }],
                want: &[],
            },
            Case {
                name: "only busy workers",
                pools: vec![HTTP],
                slots: &[
                    Slot {
                        index: 0,
                        state: SLOT_ACTIVE,
                        handled: 0,
                    },
                    Slot {
                        index: 1,
                        state: SLOT_ACTIVE,
                        handled: 0,
                    },
                ],
                want: &[],
            },
            Case {
                name: "booting and draining workers",
                pools: vec![HTTP],
                slots: &[
                    Slot {
                        index: 0,
                        state: SLOT_STARTING,
                        handled: 0,
                    },
                    Slot {
                        index: 1,
                        state: SLOT_DRAINING,
                        handled: 0,
                    },
                ],
                want: &["http"],
            },
            Case {
                name: "dead workers with counters",
                pools: vec![HTTP],
                slots: &[
                    Slot {
                        index: 0,
                        state: SLOT_FREE,
                        handled: 5,
                    },
                    Slot {
                        index: 1,
                        state: SLOT_FREE,
                        handled: 7,
                    },
                ],
                want: &["http"],
            },
            Case {
                name: "one of two pools not ready",
                pools: vec![HTTP, GRPC],
                slots: &[
                    Slot {
                        index: 0,
                        state: SLOT_IDLE,
                        handled: 0,
                    },
                    Slot {
                        index: 4,
                        state: SLOT_STARTING,
                        handled: 0,
                    },
                ],
                want: &["grpc"],
            },
        ];
        for case in cases {
            let board = rapira_scoreboard::create(8).unwrap();
            for slot in case.slots {
                board[slot.index].state.store(slot.state, Relaxed);
                board[slot.index].handled.store(slot.handled, Relaxed);
            }
            assert_eq!(unready(board, &case.pools), case.want, "{}", case.name);
        }
    }
}
