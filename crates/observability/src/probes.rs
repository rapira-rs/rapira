use rapira_scoreboard::{PoolRegion, Scoreboard, SharedSlot};

/// The pools in `pools` without a worker that can serve: no slot of the pool is idle or active.
pub(crate) fn unready(board: &Scoreboard, pools: &[PoolRegion]) -> Vec<&'static str> {
    pools
        .iter()
        .filter(|pool| {
            !board.slots()[pool.slots.clone()]
                .iter()
                .any(SharedSlot::serving)
        })
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
        struct Case {
            name: &'static str,
            pools: Vec<PoolRegion>,
            /// The index, the state and the `handled` count of each slot that the case sets. The other slots stay FREE with no counts.
            slots: &'static [(usize, u32, u64)],
            want: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "an idle worker",
                pools: vec![HTTP],
                slots: &[(0, SLOT_IDLE, 0)],
                want: &[],
            },
            Case {
                name: "only busy workers",
                pools: vec![HTTP],
                slots: &[(0, SLOT_ACTIVE, 0), (1, SLOT_ACTIVE, 0)],
                want: &[],
            },
            Case {
                name: "booting and draining workers",
                pools: vec![HTTP],
                slots: &[(0, SLOT_STARTING, 0), (1, SLOT_DRAINING, 0)],
                want: &["http"],
            },
            Case {
                name: "dead workers with counters",
                pools: vec![HTTP],
                slots: &[(0, SLOT_FREE, 5), (1, SLOT_FREE, 7)],
                want: &["http"],
            },
            Case {
                name: "one of two pools not ready",
                pools: vec![HTTP, GRPC],
                slots: &[(0, SLOT_IDLE, 0), (4, SLOT_STARTING, 0)],
                want: &["grpc"],
            },
        ];
        for case in cases {
            let board = Scoreboard::create(8).unwrap();
            for &(i, state, handled) in case.slots {
                board.slot(i).state.store(state, Relaxed);
                board.slot(i).handled.store(handled, Relaxed);
            }
            assert_eq!(unready(&board, &case.pools), case.want, "{}", case.name);
        }
    }
}
