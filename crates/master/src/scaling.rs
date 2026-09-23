pub(crate) const MAX_SPAWN_RATE: u32 = 32;

#[derive(Debug, Clone, Copy)]
pub(crate) struct DynInput {
    pub idle: usize,
    pub running: usize,
    pub min_spare: usize,
    pub max_spare: usize,
    pub max_children: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DynAction {
    KillOldestIdle,
    Spawn(usize),
    ReachedMaxChildren,
    Steady,
}

/// Mutates `spawn_rate`: reset to 1 on trim/steady/ceiling, doubled (capped) after a spawn burst.
pub(crate) fn dynamic_tick(inp: &DynInput, spawn_rate: &mut u32) -> DynAction {
    if inp.idle > inp.max_spare {
        *spawn_rate = 1;
        return DynAction::KillOldestIdle;
    }
    if inp.idle < inp.min_spare {
        let want = (inp.min_spare - inp.idle)
            .min(*spawn_rate as usize)
            .min(inp.max_children.saturating_sub(inp.running));
        if want == 0 {
            *spawn_rate = 1;
            return DynAction::ReachedMaxChildren;
        }
        *spawn_rate = spawn_rate.saturating_mul(2).min(MAX_SPAWN_RATE);
        return DynAction::Spawn(want);
    }
    *spawn_rate = 1;
    DynAction::Steady
}

/// Initial worker count of a dynamic pool: the spare midpoint, capped at `max_children`.
pub(crate) fn dynamic_start_count(
    min_spare: usize,
    max_spare: usize,
    max_children: usize,
) -> usize {
    min_spare.midpoint(max_spare).min(max_children)
}

/// Arming with an idle worker parked in accept, or a fork in flight, busy-spins level-triggered poll until the child accepts.
pub(crate) fn ondemand_armed(
    normal: bool,
    running: usize,
    max_children: usize,
    idle: usize,
    starting: usize,
) -> bool {
    normal && running < max_children && idle == 0 && starting == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp(idle: usize, running: usize) -> DynInput {
        DynInput {
            idle,
            running,
            min_spare: 2,
            max_spare: 4,
            max_children: 8,
        }
    }

    #[test]
    fn too_many_idle_trims_and_resets_rate() {
        let mut rate = 8;
        assert_eq!(
            dynamic_tick(&inp(5, 5), &mut rate),
            DynAction::KillOldestIdle
        );
        assert_eq!(rate, 1);
    }

    #[test]
    fn within_band_is_steady_and_resets_rate() {
        let mut rate = 4;
        assert_eq!(dynamic_tick(&inp(2, 2), &mut rate), DynAction::Steady);
        assert_eq!(rate, 1);
        let mut rate = 4;
        assert_eq!(dynamic_tick(&inp(4, 4), &mut rate), DynAction::Steady);
        assert_eq!(rate, 1);
    }

    #[test]
    fn below_min_spawns_capped_by_deficit_and_rate() {
        let mut rate = 1;
        assert_eq!(dynamic_tick(&inp(0, 1), &mut rate), DynAction::Spawn(1));
        assert_eq!(rate, 2);
        let mut rate = 2;
        assert_eq!(dynamic_tick(&inp(0, 1), &mut rate), DynAction::Spawn(2));
        assert_eq!(rate, 4);
    }

    #[test]
    fn spawn_rate_doubles_and_caps_at_max() {
        let mut rate = MAX_SPAWN_RATE;
        let big = DynInput {
            idle: 0,
            running: 0,
            min_spare: 100,
            max_spare: 200,
            max_children: 1000,
        };
        assert_eq!(dynamic_tick(&big, &mut rate), DynAction::Spawn(32));
        assert_eq!(rate, MAX_SPAWN_RATE);
    }

    #[test]
    fn spawn_is_bounded_by_max_children_headroom() {
        let mut rate = 8;
        let i = DynInput {
            idle: 0,
            running: 7,
            min_spare: 4,
            max_spare: 6,
            max_children: 8,
        };
        assert_eq!(dynamic_tick(&i, &mut rate), DynAction::Spawn(1));
    }

    #[test]
    fn at_max_children_reports_ceiling_and_resets_rate() {
        let mut rate = 8;
        let i = DynInput {
            idle: 0,
            running: 8,
            min_spare: 2,
            max_spare: 4,
            max_children: 8,
        };
        assert_eq!(dynamic_tick(&i, &mut rate), DynAction::ReachedMaxChildren);
        assert_eq!(rate, 1);
    }

    #[test]
    fn start_count_formula() {
        assert_eq!(dynamic_start_count(2, 6, 10), 4);
        assert_eq!(dynamic_start_count(1, 1, 10), 1);
        assert_eq!(dynamic_start_count(5, 20, 8), 8);
    }

    #[test]
    fn ondemand_arming_invariant() {
        assert!(ondemand_armed(true, 0, 4, 0, 0));
        assert!(!ondemand_armed(false, 0, 4, 0, 0));
        assert!(!ondemand_armed(true, 4, 4, 0, 0));
        assert!(!ondemand_armed(true, 1, 4, 1, 0));
        assert!(!ondemand_armed(true, 1, 4, 0, 1));
    }
}
