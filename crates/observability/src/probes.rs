use std::sync::atomic::Ordering::Relaxed;

use rapira_scoreboard::{PoolRegion, SLOT_ACTIVE, SLOT_IDLE, Scoreboard};

/// The pools without a worker that can serve: no slot of the pool is idle or active. `own` is the pool of the observability process.
pub(crate) fn unready(board: &Scoreboard, regions: &[PoolRegion], own: usize) -> Vec<&'static str> {
    regions
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != own)
        .filter(|(_, region)| {
            !board.slots()[region.slots.clone()]
                .iter()
                .any(|s| matches!(s.state.load(Relaxed), SLOT_IDLE | SLOT_ACTIVE))
        })
        .map(|(_, region)| region.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use rapira_scoreboard::{SLOT_DRAINING, SLOT_FREE, SLOT_STARTING};

    use super::*;

    const OBSERVABILITY: PoolRegion = PoolRegion {
        name: "observability",
        processes: 1,
        slots: 0..2,
    };
    const HTTP: PoolRegion = PoolRegion {
        name: "http",
        processes: 2,
        slots: 2..6,
    };
    const GRPC: PoolRegion = PoolRegion {
        name: "grpc",
        processes: 2,
        slots: 6..10,
    };

    /// The observability pool is pool 0. Its slot 0 is idle, because the observability process stores idle after its bind. The last case leaves the slot FREE.
    #[test]
    fn unready_lists_each_pool_without_a_serving_worker() {
        struct Case {
            name: &'static str,
            regions: Vec<PoolRegion>,
            /// The index, the state and the `handled` count of each slot that the case sets. The other slots stay FREE with no counts.
            slots: &'static [(usize, u32, u64)],
            want: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "an idle worker",
                regions: vec![OBSERVABILITY, HTTP],
                slots: &[(0, SLOT_IDLE, 0), (2, SLOT_IDLE, 0)],
                want: &[],
            },
            Case {
                name: "only busy workers",
                regions: vec![OBSERVABILITY, HTTP],
                slots: &[(0, SLOT_IDLE, 0), (2, SLOT_ACTIVE, 0), (3, SLOT_ACTIVE, 0)],
                want: &[],
            },
            Case {
                name: "booting and draining workers",
                regions: vec![OBSERVABILITY, HTTP],
                slots: &[
                    (0, SLOT_IDLE, 0),
                    (2, SLOT_STARTING, 0),
                    (3, SLOT_DRAINING, 0),
                ],
                want: &["http"],
            },
            Case {
                name: "dead workers with counters",
                regions: vec![OBSERVABILITY, HTTP],
                slots: &[(0, SLOT_IDLE, 0), (2, SLOT_FREE, 5), (3, SLOT_FREE, 7)],
                want: &["http"],
            },
            Case {
                name: "one of two pools not ready",
                regions: vec![OBSERVABILITY, HTTP, GRPC],
                slots: &[(0, SLOT_IDLE, 0), (2, SLOT_IDLE, 0), (6, SLOT_STARTING, 0)],
                want: &["grpc"],
            },
            Case {
                name: "the own pool is left out",
                regions: vec![OBSERVABILITY, HTTP],
                slots: &[(2, SLOT_IDLE, 0)],
                want: &[],
            },
        ];
        for case in cases {
            let board = Scoreboard::create(10).unwrap();
            for &(i, state, handled) in case.slots {
                board.slot(i).state.store(state, Relaxed);
                board.slot(i).handled.store(handled, Relaxed);
            }
            assert_eq!(
                unready(&board, &case.regions, 0),
                case.want,
                "{}",
                case.name
            );
        }
    }
}
