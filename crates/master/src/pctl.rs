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

/// Master-wide control state. Each pool drives its own reload chain while this stays `Reloading`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PctlState {
    Normal,
    Stopping { phase: KillPhase },
    Reloading,
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

    pub fn is_reloading(&self) -> bool {
        matches!(self.state, PctlState::Reloading)
    }

    /// Override precedence: normal < reloading < stopping; only TERM/INT overrides stopping (forced), while a retried QUIT stays graceful.
    pub fn on_signal(&mut self, byte: u8) -> SignalAction {
        use crate::signals::{SIG_HUP, SIG_INT, SIG_QUIT, SIG_TERM, SIG_USR1, SIG_USR2};
        match byte {
            SIG_TERM | SIG_INT | SIG_QUIT => match self.state {
                PctlState::Stopping { .. } if byte == SIG_QUIT => SignalAction::Ignore,
                PctlState::Stopping { .. } => SignalAction::Forced,
                _ => {
                    self.state = PctlState::Stopping {
                        phase: KillPhase::Quit,
                    };
                    SignalAction::Stop
                }
            },
            SIG_USR2 | SIG_HUP => match self.state {
                PctlState::Normal => {
                    self.state = PctlState::Reloading;
                    SignalAction::Reload
                }
                _ => SignalAction::Ignore,
            },
            SIG_USR1 => SignalAction::Status,
            _ => SignalAction::Ignore,
        }
    }

    /// Next signal for every worker of every pool. A reload escalates per pool, against the one worker that drains.
    pub fn escalate(&mut self) -> Option<c_int> {
        match &mut self.state {
            PctlState::Normal | PctlState::Reloading => None,
            PctlState::Stopping { phase } => Some(phase.advance()),
        }
    }

    pub fn finish_reload(&mut self) {
        self.state = PctlState::Normal;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::{SIG_HUP, SIG_INT, SIG_QUIT, SIG_TERM, SIG_USR1, SIG_USR2};

    #[test]
    fn normal_stop_signals_enter_stopping() {
        for b in [SIG_TERM, SIG_INT, SIG_QUIT] {
            let mut p = Pctl::default();
            assert_eq!(p.on_signal(b), SignalAction::Stop);
            assert_eq!(
                p.state,
                PctlState::Stopping {
                    phase: KillPhase::Quit
                }
            );
        }
    }

    #[test]
    fn second_stop_signal_is_forced() {
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_TERM), SignalAction::Stop);
        assert_eq!(p.on_signal(SIG_TERM), SignalAction::Forced);
        assert_eq!(p.on_signal(SIG_INT), SignalAction::Forced);
    }

    #[test]
    fn retried_quit_stays_graceful() {
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_QUIT), SignalAction::Stop);
        assert_eq!(p.on_signal(SIG_QUIT), SignalAction::Ignore);
        assert!(p.is_stopping());
        assert_eq!(p.on_signal(SIG_TERM), SignalAction::Forced);
    }

    #[test]
    fn reload_from_normal_only() {
        for b in [SIG_USR2, SIG_HUP] {
            let mut p = Pctl::default();
            assert_eq!(p.on_signal(b), SignalAction::Reload);
            assert_eq!(p.state, PctlState::Reloading);
        }
    }

    #[test]
    fn reload_ignored_while_stopping_or_reloading() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        assert_eq!(p.on_signal(SIG_USR2), SignalAction::Ignore);
        assert_eq!(p.on_signal(SIG_HUP), SignalAction::Ignore);

        let mut p = Pctl::default();
        p.on_signal(SIG_TERM);
        assert_eq!(p.on_signal(SIG_USR2), SignalAction::Ignore);
        assert!(p.is_stopping());
    }

    #[test]
    fn stop_overrides_reload() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        assert!(p.is_reloading());
        assert_eq!(p.on_signal(SIG_TERM), SignalAction::Stop);
        assert!(p.is_stopping());
    }

    #[test]
    fn status_is_stateless() {
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_USR1), SignalAction::Status);
        assert_eq!(p.state, PctlState::Normal);
        p.on_signal(SIG_TERM);
        assert_eq!(p.on_signal(SIG_USR1), SignalAction::Status);
        assert!(p.is_stopping());
    }

    #[test]
    fn stopping_escalation_phase_progression() {
        let mut p = Pctl::default();
        p.on_signal(SIG_TERM);
        assert_eq!(p.escalate(), Some(libc::SIGTERM));
        assert_eq!(p.escalate(), Some(libc::SIGKILL));
        assert_eq!(p.escalate(), Some(libc::SIGKILL));
    }

    #[test]
    fn normal_has_no_escalation() {
        let mut p = Pctl::default();
        assert_eq!(p.escalate(), None);
    }

    /// Escalation against a draining worker belongs to its pool; the global machine sends nothing.
    #[test]
    fn reloading_has_no_global_escalation() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        assert_eq!(p.escalate(), None);
        assert!(p.is_reloading());
    }

    #[test]
    fn finish_reload_returns_to_normal() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        p.finish_reload();
        assert_eq!(p.state, PctlState::Normal);
    }
}
