use std::time::{Duration, Instant};

use libc::c_int;

/// Escalation phase: the signal already sent; the next deadline advances it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillPhase {
    Quit,
    Term,
    Kill,
}

impl KillPhase {
    pub(crate) fn advance(&mut self) -> c_int {
        match self {
            KillPhase::Quit => {
                *self = KillPhase::Term;
                libc::SIGTERM
            }
            KillPhase::Term => {
                *self = KillPhase::Kill;
                libc::SIGKILL
            }
            KillPhase::Kill => libc::SIGKILL,
        }
    }
}

/// Master-wide control state. Each pool owns its reload chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PctlState {
    Normal,
    /// `deadline` is when the next escalation fires.
    Stopping {
        phase: KillPhase,
        deadline: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalAction {
    Stop,
    Forced,
    Reload,
    Status,
    Ignore,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Pctl {
    pub state: PctlState,
}

impl Default for Pctl {
    fn default() -> Self {
        Pctl {
            state: PctlState::Normal,
        }
    }
}

impl Pctl {
    pub fn is_stopping(&self) -> bool {
        matches!(self.state, PctlState::Stopping { .. })
    }

    /// `stop_deadline` arms the first escalation when this signal starts a stop.
    /// Override precedence: normal < stopping; only TERM/INT overrides stopping (forced), while a retried QUIT stays graceful.
    pub fn on_signal(&mut self, byte: u8, stop_deadline: Instant) -> SignalAction {
        use crate::signals::{SIG_HUP, SIG_INT, SIG_QUIT, SIG_TERM, SIG_USR1, SIG_USR2};
        match byte {
            SIG_TERM | SIG_INT | SIG_QUIT => match self.state {
                PctlState::Stopping { .. } if byte == SIG_QUIT => SignalAction::Ignore,
                PctlState::Stopping { .. } => SignalAction::Forced,
                _ => {
                    self.state = PctlState::Stopping {
                        phase: KillPhase::Quit,
                        deadline: stop_deadline,
                    };
                    SignalAction::Stop
                }
            },
            SIG_USR2 | SIG_HUP => match self.state {
                PctlState::Normal => SignalAction::Reload,
                PctlState::Stopping { .. } => SignalAction::Ignore,
            },
            SIG_USR1 => SignalAction::Status,
            _ => SignalAction::Ignore,
        }
    }

    pub fn stop_deadline(&self) -> Option<Instant> {
        match self.state {
            PctlState::Normal => None,
            PctlState::Stopping { deadline, .. } => Some(deadline),
        }
    }

    /// Runs only at the stop deadline: re-arms it one second out and returns the next signal for every worker of every pool. A reload escalates per pool, against the one worker that drains.
    pub fn escalate(&mut self, now: Instant) -> c_int {
        let PctlState::Stopping { phase, deadline } = &mut self.state else {
            unreachable!("stop escalation outside a stop");
        };
        *deadline = now + Duration::from_secs(1);
        phase.advance()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::{SIG_HUP, SIG_INT, SIG_QUIT, SIG_TERM, SIG_USR1, SIG_USR2};

    #[test]
    fn normal_stop_signals_enter_stopping() {
        let t0 = Instant::now();
        for b in [SIG_TERM, SIG_INT, SIG_QUIT] {
            let mut p = Pctl::default();
            assert_eq!(p.on_signal(b, t0), SignalAction::Stop);
            assert_eq!(
                p.state,
                PctlState::Stopping {
                    phase: KillPhase::Quit,
                    deadline: t0
                }
            );
        }
    }

    #[test]
    fn second_stop_signal_is_forced() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_TERM, t0), SignalAction::Stop);
        assert_eq!(p.on_signal(SIG_TERM, t0), SignalAction::Forced);
        assert_eq!(p.on_signal(SIG_INT, t0), SignalAction::Forced);
    }

    #[test]
    fn retried_quit_stays_graceful() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_QUIT, t0), SignalAction::Stop);
        assert_eq!(p.on_signal(SIG_QUIT, t0), SignalAction::Ignore);
        assert!(p.is_stopping());
        assert_eq!(p.on_signal(SIG_TERM, t0), SignalAction::Forced);
    }

    #[test]
    fn reload_from_normal_only() {
        let t0 = Instant::now();
        for b in [SIG_USR2, SIG_HUP] {
            let mut p = Pctl::default();
            assert_eq!(p.on_signal(b, t0), SignalAction::Reload);
            assert_eq!(p.state, PctlState::Normal);
        }
    }

    #[test]
    fn reload_ignored_while_stopping() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        p.on_signal(SIG_TERM, t0);
        assert_eq!(p.on_signal(SIG_USR2, t0), SignalAction::Ignore);
        assert!(p.is_stopping());
    }

    #[test]
    fn status_is_stateless() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_USR1, t0), SignalAction::Status);
        assert_eq!(p.state, PctlState::Normal);
        p.on_signal(SIG_TERM, t0);
        assert_eq!(p.on_signal(SIG_USR1, t0), SignalAction::Status);
        assert!(p.is_stopping());
    }

    #[test]
    fn stopping_escalation_phase_progression() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        p.on_signal(SIG_TERM, t0);
        assert_eq!(p.escalate(t0), libc::SIGTERM);
        assert_eq!(p.escalate(t0), libc::SIGKILL);
        assert_eq!(p.escalate(t0), libc::SIGKILL);
    }
}
