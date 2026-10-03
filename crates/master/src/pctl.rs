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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Pctl {
    #[default]
    Normal,
    /// `deadline` is when the next escalation fires.
    Stopping { phase: KillPhase, deadline: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalAction {
    Stop,
    Forced,
    Reload,
    Status,
    Ignore,
}

impl Pctl {
    pub fn is_stopping(&self) -> bool {
        matches!(self, Pctl::Stopping { .. })
    }

    /// `stop_deadline` arms the first escalation when this signal starts a stop.
    /// Override precedence: normal < stopping; only TERM/INT overrides stopping (forced), while a retried QUIT stays graceful.
    pub fn on_signal(&mut self, signo: c_int, stop_deadline: Instant) -> SignalAction {
        match signo {
            libc::SIGTERM | libc::SIGINT | libc::SIGQUIT => match *self {
                Pctl::Stopping { .. } if signo == libc::SIGQUIT => SignalAction::Ignore,
                Pctl::Stopping { .. } => SignalAction::Forced,
                Pctl::Normal => {
                    *self = Pctl::Stopping {
                        phase: KillPhase::Quit,
                        deadline: stop_deadline,
                    };
                    SignalAction::Stop
                }
            },
            libc::SIGUSR2 | libc::SIGHUP => match *self {
                Pctl::Normal => SignalAction::Reload,
                Pctl::Stopping { .. } => SignalAction::Ignore,
            },
            libc::SIGUSR1 => SignalAction::Status,
            _ => SignalAction::Ignore,
        }
    }

    pub fn stop_deadline(&self) -> Option<Instant> {
        match self {
            Pctl::Normal => None,
            Pctl::Stopping { deadline, .. } => Some(*deadline),
        }
    }

    /// Runs only at the stop deadline: re-arms it one second out and returns the next signal for every worker of every pool. A reload escalates per pool, against the one worker that drains.
    pub fn escalate(&mut self, now: Instant) -> c_int {
        let Pctl::Stopping { phase, deadline } = self else {
            unreachable!("stop escalation outside a stop");
        };
        *deadline = now + Duration::from_secs(1);
        phase.advance()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_stop_signals_enter_stopping() {
        let t0 = Instant::now();
        for signo in [libc::SIGTERM, libc::SIGINT, libc::SIGQUIT] {
            let mut p = Pctl::default();
            assert_eq!(p.on_signal(signo, t0), SignalAction::Stop);
            assert_eq!(
                p,
                Pctl::Stopping {
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
        assert_eq!(p.on_signal(libc::SIGTERM, t0), SignalAction::Stop);
        assert_eq!(p.on_signal(libc::SIGTERM, t0), SignalAction::Forced);
        assert_eq!(p.on_signal(libc::SIGINT, t0), SignalAction::Forced);
    }

    #[test]
    fn retried_quit_stays_graceful() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(libc::SIGQUIT, t0), SignalAction::Stop);
        assert_eq!(p.on_signal(libc::SIGQUIT, t0), SignalAction::Ignore);
        assert!(p.is_stopping());
        assert_eq!(p.on_signal(libc::SIGTERM, t0), SignalAction::Forced);
    }

    #[test]
    fn reload_from_normal_only() {
        let t0 = Instant::now();
        for signo in [libc::SIGUSR2, libc::SIGHUP] {
            let mut p = Pctl::default();
            assert_eq!(p.on_signal(signo, t0), SignalAction::Reload);
            assert_eq!(p, Pctl::Normal);
        }
    }

    #[test]
    fn reload_ignored_while_stopping() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        p.on_signal(libc::SIGTERM, t0);
        assert_eq!(p.on_signal(libc::SIGUSR2, t0), SignalAction::Ignore);
        assert!(p.is_stopping());
    }

    #[test]
    fn status_is_stateless() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(libc::SIGUSR1, t0), SignalAction::Status);
        assert_eq!(p, Pctl::Normal);
        p.on_signal(libc::SIGTERM, t0);
        assert_eq!(p.on_signal(libc::SIGUSR1, t0), SignalAction::Status);
        assert!(p.is_stopping());
    }

    #[test]
    fn stopping_escalation_phase_progression() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        p.on_signal(libc::SIGTERM, t0);
        assert_eq!(p.escalate(t0), libc::SIGTERM);
        assert_eq!(p.escalate(t0), libc::SIGKILL);
        assert_eq!(p.escalate(t0), libc::SIGKILL);
    }
}
